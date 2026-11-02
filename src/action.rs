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
//! TMPDIR is the client's TMPDIR, or `/tmp` when it is unset or empty, under
//! every strategy, replacing any TMPDIR of the action environment
//! (PosixLocalEnvProvider.rewriteLocalEnv,
//! exec/local/PosixLocalEnvProvider.java:41-59; [`local_tmpdir`]); like
//! Bazel's, it is shared between actions and not keyed. The working
//! directory's spelling is intentionally not stable, so actions must not
//! embed it in output.
//!
//! Tree artifacts (declared directories, docs/ACTIONS_PARAMS.md) are directory
//! outputs and inputs: their regular files and subdirectories are keyed,
//! staged and published. A symlink inside a tree output is followed to the
//! file or directory it resolves to, like Bazel's TreeArtifactValue.visitTree
//! (docs/TREE_SYMLINKS.md): the tree holds what it resolved to at its own
//! path, and publication materializes it (a published tree has no symlinks).
//! Source directories (source artifacts naming a directory,
//! docs/SOURCE_DIRECTORIES.md) are directory inputs keyed by content like
//! Bazel's tracked source directories: symlinks below them are followed,
//! dangling ones kept, and each is read once per process.
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
//!   establish that they are real directories (`no_linked_ancestor`);
//! - a file's digest is remembered only for a state this process observed
//!   (`DigestMemo`): a settled state (timestamps at least 2 s older than
//!   the read, crate::digest_memo::RACY_MARGIN) for every path of its
//!   inode, a racy one only at the path where this process established the
//!   content — a store blob it validated or created, an output it
//!   published — until it removes or replaces that path. A stat key alone
//!   never vouches for a racy file: a file deleted and replaced by one of
//!   the same size inside one timestamp tick passes its inode number and
//!   timestamps on.
//!
//! Only bazel-rs writes these areas, so an edit made there while a command
//! runs, after its first look, is not seen again by that command (the next
//! one starts with empty per-process memos, re-verifies the repository trees
//! it materializes and takes repository tree files from the persistent
//! digest memo only by their change time); staging still checks the digest
//! of every blob it links, and of a repository tree file it links an
//! executable to (`stage_tree_link`) (docs/HARDLINK_STAGING.md, "Per-process
//! memos"; across commands: docs/DIGEST_MEMO.md, "Trust").
//! Workspace files and every other path outside the cache keep the full
//! checks on every use. Many inputs (or tree files) are read and hashed on
//! several threads (`try_map_ordered`, helpers from one process-wide budget
//! of as many threads as cores), reporting the error a sequential read
//! reports.

use crate::fast_hash::{FxHashMap, FxHashSet};
use crate::shared_path::SharedPath;
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
use std::process::Stdio;
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
// N36) moved to v8 with the key, so v7 entries rebuilt once.
// `native-action-v10` (snapshot-keys, SURVEY3 S1): the action's id (its
// owner) is no longer keyed, its mnemonic is (as in SpawnAction.computeKey);
// every key changed, so v8 entries rebuild once (the manifests stay v9).
const FORMAT: &str = "native-action-v10";
/// The format of the published entries' manifests: `native-action-v9`
/// (fix-digest-memo; the key was `native-action-v8` then, FORMAT). Binaries
/// that wrote v8 manifests could publish another action's output — their
/// per-process digest memo answered for a new file with the digest of a
/// deleted one (docs/DIGEST_MEMO.md, "Upgrade note") — and such an entry is
/// self-consistent (its manifest names the bytes it holds), so no check of
/// it could tell. An entry of any other format is a cache miss, never an
/// error: the action runs once more and its entry is replaced.
const MANIFEST_FORMAT: &str = "native-action-v9";
/// The content-addressed store below the cache root.
const CAS: &str = "cas";
/// Read and copy buffer for streamed digests.
const BUFFER_BYTES: usize = 1 << 20;
// Caps of the per-process memos. A memo starts over when it would exceed
// its cap, which is always safe: a memo only saves work, and consumers keep
// what they took from it (shared trees are Arcs, inputs are copies). Their
// worst-case footprints at the caps (32-byte digests, absolute paths of
// 160-230 bytes as on ~/code, hash-table and B-tree slack included): digest
// memo ~45 MB of settled states plus ~85 MB of produced files, tree memo
// ~440 MB (1 Mi entries), file memo ~85 MB, output memo ~110 MB,
// directory memos ~60 MB each. The warm `oauth2token/...` build (689
// actions) fills about 3% of them: 8.9k digests, 958 tree entries, 7.4k
// store files, 7.3k directories; the warm `//src/ts/...` build (24,608
// actions) holds ~430k tree entries and ~19k published outputs.
/// Settled file states with a known digest (88 bytes each, no allocation),
/// and files the process produced (a path and 88 bytes each).
const DIGEST_MEMO_LIMIT: usize = 1 << 18;
/// Entries (files plus directories) of memoized published trees. A
/// TypeScript build's `node_modules` package trees hold ~430k (~/code
/// `//src/ts/...`): a smaller memo starts over during a warm build and its
/// consumers walk the trees again.
const TREE_MEMO_LIMIT: usize = 1 << 20;
/// Remembered cache-owned real directories (one path each).
const DIRECTORY_MEMO_LIMIT: usize = 1 << 18;
/// Remembered immutable-store files (a path and 48 bytes each).
const FILE_MEMO_LIMIT: usize = 1 << 18;
/// Remembered published outputs (a path, and an input or a link text).
const OUTPUT_MEMO_LIMIT: usize = 1 << 18;
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

/// Encoded raw (paths as bytes) in a planned request (crate::snapshot::codec).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Action {
    pub id: String,
    pub command: String,
    /// Source inputs (path, destination), shared with the artifacts they
    /// list (crate::shared_path).
    #[serde(with = "crate::snapshot::codec::path_pairs")]
    pub inputs: Vec<(SharedPath, SharedPath)>,
    #[serde(with = "crate::snapshot::codec::paths")]
    pub outputs: Vec<PathBuf>,
    /// Native write, mutually exclusive with `program` and shell commands.
    pub write: Option<WriteAction>,
    /// Direct executable; `None` selects Bash unless this is a native write.
    pub program: Option<String>,
    /// Literal process arguments, or shell positional parameters starting at `$1`.
    pub arguments: Vec<String>,
    /// The fixed part of the action environment (a TMPDIR entry is keyed
    /// but replaced by the local TMPDIR, [`local_tmpdir`]).
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
    /// (crate::builtin_tools::GENRULE_SETUP); encoded as the blob's tag.
    #[serde(with = "crate::snapshot::codec::embedded")]
    pub embedded_inputs: Vec<(PathBuf, &'static [u8])>,
    /// Why this spawn's remote cache key cannot equal Bazel's (an execution
    /// platform whose properties analysis did not resolve); such spawns
    /// never use the remote cache or remote execution.
    pub remote_unsupported: Option<String>,
    /// The spawn's exec properties and execution platform, for its REAPI
    /// `Platform` (PlatformUtils.getPlatformProto,
    /// crate::remote_execution::platform_properties). Not part of the
    /// native key.
    pub remote_platform: crate::remote_execution::SpawnPlatform,
    /// Invocation-local cancellation at Java attempt boundaries. Never keyed
    /// or persisted; the scheduler installs its existing failure flag.
    #[serde(skip)]
    pub execution_cancel: Option<Arc<std::sync::atomic::AtomicBool>>,
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

/// `--subcommands` (ExecutionOptions.showSubcommands, ShowSubcommands
/// FALSE/TRUE/PRETTY_PRINT): 0, 1 or 2.
static SUBCOMMANDS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// `--subcommands` / `-s` (BoolOrEnumConverter of ShowSubcommands,
/// ExecutionOptions.java:642-647): `Some(Ok(mode))` when `arg` is one
/// (0 false, 1 true, 2 pretty_print).
pub fn parse_subcommands(arg: &str) -> Option<Result<u8, String>> {
    match arg {
        "-s" => return Some(Ok(1)),
        "-s-" => return Some(Ok(0)),
        _ => {}
    }
    let rest = arg.strip_prefix("--")?;
    if let Some(value) = rest.strip_prefix("subcommands=") {
        return Some(match value.to_ascii_lowercase().as_str() {
            "pretty_print" => Ok(2),
            "true" | "1" | "yes" | "t" | "y" => Ok(1),
            "false" | "0" | "no" | "f" | "n" => Ok(0),
            _ => Err(format!(
                "While parsing option {arg}: Not a valid subcommand option: '{value}' (should be true, pretty_print or false)"
            )),
        });
    }
    crate::configuration::boolean_option(arg, "subcommands").map(|value| value.map(u8::from))
}

/// Install this command's `--subcommands` mode.
pub fn set_subcommands(mode: u8) {
    SUBCOMMANDS.store(mode, Ordering::Relaxed);
}

/// ActionExecutionContext.maybeReportSubcommand (:459-499) with
/// CommandFailureUtils.describeCommand's COMPLETE form (:76-170) and the
/// Linux ScriptUtil emitter: a SUBCOMMAND event before a spawn reaches its
/// runner. Native actions carry no ActionOwner, progress message or
/// configuration checksum: the owner is the action's label, the progress
/// text `MNEMONIC primary-output` (Bazel's for genrules, `Executing genrule
/// L`), the configuration the output directory name, and no execution
/// platform is printed; `cwd` is the spawn's own execution root.
fn report_subcommand(action: &Action, execroot: &Path, runner: &str) {
    let mode = SUBCOMMANDS.load(Ordering::Relaxed);
    if mode == 0 {
        return;
    }
    let label = action.id.split('#').next().unwrap_or(&action.id);
    let mnemonic = action.extra.mnemonic.as_str();
    let primary = action
        .outputs
        .first()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let progress = if mnemonic == "Genrule" {
        format!("Executing genrule {label}")
    } else {
        format!("{mnemonic} {primary}")
    };
    let configuration = primary
        .strip_prefix("bazel-out/")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("");
    let escape = crate::args::shell_escape;
    let mut message = format!(
        "# {label} [action '{progress}', configuration: {configuration}, mnemonic: {mnemonic}]\n"
    );
    message.push_str(&format!(
        "(cd {} && \\\n  exec env - \\\n  ",
        escape(&execroot.display().to_string())
    ));
    for (name, value) in resolved_environment(action) {
        message.push_str(&format!(
            "  {}={} \\\n  ",
            escape(&name),
            escape(&value.to_string_lossy())
        ));
    }
    let separator = if mode == 2 { " \\\n    " } else { " " };
    let argv = arguments(action);
    message.push_str(
        &argv
            .iter()
            .map(|argument| escape(argument))
            .collect::<Vec<_>>()
            .join(separator),
    );
    message.push(')');
    message.push_str(&format!("\n# Configuration: {configuration}"));
    message.push_str(&format!("\n# Runner: {runner}"));
    crate::note!("SUBCOMMAND: {message}");
}

/// Spawns.mayBeCachedRemotely and mayBeExecutedRemotely
/// (actions/Spawns.java:34-56) over the rule tags and the action's
/// execution requirements.
/// `Spawns.mayBeCached`: no `no-cache` and no `local` (the disk cache's
/// policy; [`remote_policy`]'s first value adds `no-remote` and
/// `no-remote-cache`).
pub fn may_be_cached(action: &Action) -> bool {
    let has = |key: &str| {
        action.execution_info.contains(key) || action.extra.execution_info.contains_key(key)
    };
    !has("no-cache") && !has("local")
}

/// The write policy of a locally executed spawn's results: the spawn
/// cache's, or after a remote execution's `--remote_local_fallback`
/// (RemoteSpawnRunner.execLocallyAndUpload) the remote cache's under
/// `--remote_upload_local_results`.
fn remote_write(
    remote_exec: bool,
    fallback: bool,
    write: crate::remote_cache::CachePolicy,
) -> crate::remote_cache::CachePolicy {
    if remote_exec && fallback {
        crate::remote_cache::CachePolicy {
            remote: crate::remote_cache::active().is_some_and(|remote| remote.upload_local_results),
            disk: false,
        }
    } else {
        write
    }
}

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
/// local TMPDIR (PosixLocalEnvProvider.rewriteLocalEnv, [`local_tmpdir`]). The
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

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ActionExtra {
    /// Outputs (members of `Action::outputs`) that are tree artifacts.
    #[serde(with = "crate::snapshot::codec::path_set")]
    pub tree_outputs: BTreeSet<PathBuf>,
    /// Outputs (members of `Action::outputs`) that are unresolved symlink
    /// artifacts (ctx.actions.declare_symlink): the action leaves a symlink
    /// there whose literal link text is the artifact's value, like Bazel's
    /// UnresolvedSymlinkArtifactValue (docs/UNRESOLVED_SYMLINKS.md).
    #[serde(with = "crate::snapshot::codec::path_set")]
    pub symlink_outputs: BTreeSet<PathBuf>,
    /// Input destinations that are unresolved symlink artifacts: read with
    /// readlink (never followed), keyed by their link text and staged as
    /// symlinks with that text (SandboxHelpers.processInputFiles,
    /// SymlinkTreeHelper.createRunfilesSymlinks).
    #[serde(with = "crate::snapshot::codec::path_set")]
    pub symlink_inputs: BTreeSet<PathBuf>,
    /// Input destinations that are tree artifacts (directories); every other
    /// input must be a regular file or a source directory.
    #[serde(with = "crate::snapshot::codec::path_set")]
    pub tree_inputs: BTreeSet<PathBuf>,
    /// Input destinations that are source directories: source artifacts
    /// naming a directory (a package's directory used as a file target, an
    /// external repository's directory such as rules_js's npm `package`),
    /// keyed by everything below them like Bazel 9.3's tracked source
    /// directories (ArtifactFunction.createSourceValue,
    /// TrackSourceDirectoriesFlag) and staged as a real directory of their
    /// files ([`SourceDirectory`], docs/SOURCE_DIRECTORIES.md).
    #[serde(with = "crate::snapshot::codec::path_set")]
    pub source_directories: BTreeSet<PathBuf>,
    /// Parameter files (CommandLines.ParamFileActionInput): written below
    /// the execution root before the process starts; content-keyed.
    #[serde(with = "crate::snapshot::codec::path_contents")]
    pub param_files: Vec<(PathBuf, Vec<u8>)>,
    /// The inherited part of the action environment: names whose values
    /// come from the client environment (ActionEnvironment.getInheritedEnv).
    pub inherited_environment: BTreeSet<String>,
    /// The client values of the inherited names that the client
    /// environment sets, resolved for each run by the planned request's
    /// finalize (crate::request; ActionEnvironment::resolve_inherited);
    /// keyed, and they override fixed values of the same name. Never
    /// encoded: a snapshot holds no environment values.
    #[serde(skip)]
    pub client_environment: BTreeMap<String, std::ffi::OsString>,
    /// Execution requirements: keyed, no local effect.
    pub execution_info: BTreeMap<String, String>,
    /// The spawn mnemonic (Spawn.getMnemonic): "Genrule" for genrule actions
    /// (GenRuleAction.java:35), "TestRunner" for test spawns
    /// (TestRunnerAction.java:112), the Starlark `mnemonic` parameter or
    /// "Action" for ctx.actions spawns (StarlarkActionFactory.java:811).
    /// Keyed like SpawnAction.computeKey's `fp.addString(mnemonic)` (in
    /// place of the action's id since `native-action-v10`); --strategy
    /// matches it (SpawnStrategyRegistry.getStrategies:142-143), and that
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
    #[serde(with = "crate::snapshot::codec::paths")]
    pub constant_metadata: Vec<PathBuf>,
    /// JavaBuilder's semantic intent, distinct from either staged attempt.
    pub java_compile: Option<Box<crate::java_compile::JavaCompileSpec>>,
}

/// A runfiles mapping: each runfiles path with its input's execution path
/// (`None` for an empty file). One executable's manifest and tree actions
/// share one list, and its paths with the planner's runfiles entries
/// (MEM4).
pub type RunfilesEntries = std::sync::Arc<[(std::sync::Arc<str>, Option<SharedPath>)]>;

/// SymlinkTargetType (vfs/SymlinkTargetType.java): the `target_type` of an
/// unresolved symlink, part of UnresolvedSymlinkAction's key by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SymlinkTargetType {
    Unspecified,
    File,
    Directory,
}

impl SymlinkTargetType {
    /// The enum constant's `name()`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Unspecified => "UNSPECIFIED",
            Self::File => "FILE",
            Self::Directory => "DIRECTORY",
        }
    }
}

/// The link text UnresolvedSymlinkAction creates for `target`: Bazel's
/// `PathFragment.create(target)` (UnresolvedSymlinkAction
/// .getTargetPathFragment, analysis/starlark/UnresolvedSymlinkAction.java;
/// vfs/UnixOsPathPolicy.normalize): repeated and trailing separators and
/// `.` segments dropped, `x/..` pairs removed, leading `..` segments of a
/// relative path kept and those of an absolute path dropped.
pub fn unresolved_link_text(target: &str) -> String {
    if target.is_empty() {
        return String::new();
    }
    let absolute = target.starts_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for segment in target.split('/').filter(|segment| !segment.is_empty()) {
        match segment {
            "." => {}
            ".." if segments.last().is_some_and(|last| *last != "..") => {
                segments.pop();
            }
            ".." if absolute => {}
            _ => segments.push(segment),
        }
    }
    let joined = segments.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// The digest of an unresolved symlink artifact's value, Bazel's
/// UnresolvedSymlinkArtifactValue (actions/FileArtifactValue.java:1145-1168):
/// the SHA-256 of the link text (ISO-8859-1, i.e. its bytes) with the first
/// byte inverted, so that it never equals a file holding that text.
fn unresolved_symlink_digest(text: &[u8]) -> Sha256Digest {
    let mut digest = digest(text);
    digest.0[0] ^= 0xff;
    digest
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Transform {
    /// SymlinkAction: the output is a real symlink to the target's absolute
    /// input path, so it observes the target's bytes, permission bits and
    /// identity (SymlinkAction.java:218-238).
    Symlink {
        #[serde(with = "crate::snapshot::codec::path")]
        target: PathBuf,
        require_executable: bool,
        /// The expanded progress message that prefixes execution failures
        /// (`<progress message> failed: ...`); not keyed, like Bazel.
        progress: String,
    },
    /// TemplateExpansionAction: literal, sequential replacement of internal
    /// bytes (LocalTemplateExpansionStrategy.getExpandedTemplateUnsafe).
    Template {
        #[serde(with = "crate::snapshot::codec::path")]
        template: PathBuf,
        #[serde(with = "crate::snapshot::codec::byte_pairs")]
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
        #[serde(with = "crate::snapshot::codec::path")]
        template: PathBuf,
        #[serde(with = "crate::snapshot::codec::byte_pairs")]
        substitutions: Vec<(Vec<u8>, Vec<u8>)>,
        #[serde(with = "crate::snapshot::codec::byte_pairs")]
        keyed_substitutions: Vec<(Vec<u8>, Vec<u8>)>,
        executable: bool,
        volatile: bool,
        callback_digest: String,
    },
    /// An action whose execution is an explicit native gap (its outputs are
    /// declared and registered, e.g. workspace-status transforms).
    Unsupported { message: String },
    /// A workspace status node of a planned request (L2-W4): the stable or
    /// volatile status file, whose content is this run's workspace status.
    /// The request's finalize (crate::request) replaces it with the write
    /// of the computed status, or the failing node of a failed status; it
    /// never executes.
    WorkspaceStatus { volatile: bool },
    /// UnresolvedSymlinkAction (analysis/actions/UnresolvedSymlinkAction.java,
    /// ctx.actions.symlink(target_path = ...)): the only output, an
    /// unresolved symlink artifact, is a symlink with the literal `target`;
    /// keyed by the target and its type (computeKey :92-99), no inputs.
    /// The link text is [`unresolved_link_text`] of `target`.
    UnresolvedSymlink {
        target: String,
        target_type: SymlinkTargetType,
        /// The expanded progress message (not keyed, like Bazel).
        progress: String,
    },
    /// SourceManifestAction (SOURCE_SYMLINKS, analysis/SourceManifestAction.java):
    /// the `<executable>.runfiles_manifest` lines `path target` of a runfiles
    /// tree, sorted by path. Each entry names the input destination it links
    /// to (the target is that input's absolute path) or `None` for an empty
    /// file. Inputs are not read: like Bazel, the manifest records paths only.
    RunfilesManifest {
        #[serde(with = "crate::snapshot::codec::entries")]
        entries: RunfilesEntries,
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
        #[serde(with = "crate::snapshot::codec::entries")]
        entries: RunfilesEntries,
        #[serde(with = "crate::snapshot::codec::path")]
        manifest: PathBuf,
        #[serde(with = "crate::snapshot::codec::optional_path")]
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

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WriteAction {
    #[serde(with = "crate::snapshot::codec::bytes")]
    pub content: Vec<u8>,
    pub executable: bool,
}

#[derive(Debug)]
pub struct ActionResult {
    pub outputs: Vec<PathBuf>,
    pub cache_hit: bool,
    /// Outputs came from the remote cache (crate::remote_cache).
    pub remote_hit: bool,
    /// The spawn ran on the remote executor (crate::remote_execution).
    pub remote_executed: bool,
    /// Declaration-order output index of each tree output: its regular files,
    /// relative to the tree root, in byte order (TreeArtifactValue children).
    pub trees: BTreeMap<usize, Vec<PathBuf>>,
    /// The SpawnResult runner name of the spawn that produced the outputs
    /// (SpawnStats, `INFO: N processes: …`): `linux-sandbox`,
    /// `processwrapper-sandbox`, `local`, `worker`, `remote`, `remote cache
    /// hit`; `None` for an action that ran no spawn (a write, a native
    /// transform: Bazel's `internal`) and for a cache hit.
    pub runner: Option<&'static str>,
    /// Additional spawns with the same runner within this logical action
    /// (JavaBuilder's successful reduced-classpath fallback). Cache hits have
    /// none. Kept separate from the one logical completion/result.
    pub additional_spawns: u8,
    /// The critical path up to and including this action
    /// (CriticalPathComputer): its execution time plus the longest of its
    /// producers'; set by crate::scheduler.
    pub critical: std::time::Duration,
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
    /// The entries that are symlinks, each followed to the file or
    /// directory it resolves to (TreeArtifactValue.visitTree). A file at or
    /// below one ([`through_link`]) has the canonical path of that file as
    /// its `Input::path`; every other file is read at its own path. Empty
    /// for a published tree: publication materializes the links
    /// ([`publish_tree`]).
    links: BTreeSet<PathBuf>,
}

/// Whether `name` (a tree entry) is one of `links` or lies below one: its
/// path in the tree goes through a symlink.
fn through_link(links: &BTreeSet<PathBuf>, name: &Path) -> bool {
    !links.is_empty() && name.ancestors().any(|ancestor| links.contains(ancestor))
}

/// How [`read_tree`] treats the entries below a tree root.
#[derive(Clone, Copy)]
enum TreeRead<'a> {
    /// A published tree: regular files and directories only, as publication
    /// leaves them; a symlink there is damage.
    Published,
    /// Any other tree input (a source or repository directory): symlinks are
    /// followed like an output's.
    Input,
    /// A tree output in its action's execution root once the action ran.
    Output(OutputTree<'a>),
}

/// A tree output as ActionOutputMetadataStore.constructTreeArtifactValueFrom
/// Filesystem (skyframe/ActionOutputMetadataStore.java:245-316) reads it:
/// symlinks are followed (TreeArtifactValue.visitTree,
/// skyframe/TreeArtifactValue.java:548-592), and every
/// entry that no symlink led to is made OUTPUT_MODE before it is read
/// (`chmod && !traversedSymlink`), so a link's target keeps its own mode.
#[derive(Clone, Copy)]
struct OutputTree<'a> {
    /// Artifact.prettyPrint of the tree (its root-relative path), as in
    /// Bazel's `error while validating output tree artifact` errors.
    pretty: &'a str,
    /// The action's working directory. Genuine collects a tree after its
    /// spawn's sandbox and temporary directory are gone
    /// (AbstractSandboxSpawnRunner.runSpawn deletes the sandbox before it
    /// returns; LocalSpawnRunner its temporary directory), so a link with
    /// an absolute target in there dangles, ...
    work: &'a Path,
    /// ... except into the execution root of an unsandboxed spawn, which is
    /// genuine's own execution root (the spawn's working directory).
    execroot: Option<&'a Path>,
}

/// A source directory input as Bazel 9.3 tracks it (source directory
/// tracking is on by default in Bazel, TrackSourceDirectoriesFlag:
/// ArtifactFunction.createSourceValue fingerprints what
/// RecursiveFilesystemTraversalFunction finds below the directory): every
/// regular file and directory below it, symlinks followed — a link to a file
/// is that file at the link's path, a link to a directory is walked at the
/// link's path — and the dangling symlinks, tracked by their link text
/// (ResolvedFileFactory.danglingSymlink). Read once per process
/// ([`read_source_directory`]), like Bazel's per-build artifact value.
#[derive(Clone, PartialEq, Eq)]
struct SourceDirectory {
    /// Regular files by path relative to the directory; a file a symlink
    /// led to is read at its canonical path (the target's content and
    /// executable bits).
    files: BTreeMap<PathBuf, Input>,
    /// Every directory below the root, empty ones included
    /// (DirectoryArtifactTraversalRequest.emitEmptyDirectoryNodes).
    directories: BTreeSet<PathBuf>,
    /// Dangling symlinks: relative path -> link text.
    dangling: BTreeMap<PathBuf, PathBuf>,
}

#[derive(Clone, PartialEq, Eq)]
enum Staged {
    File(Input),
    /// Shared: a published tree is read once per process and every consumer
    /// stages the same value ([`read_staged`]).
    Tree(Arc<Tree>),
    /// Shared like a tree: a source directory is read once per process.
    SourceDirectory(Arc<SourceDirectory>),
    /// An unresolved symlink artifact: its literal link text, never
    /// followed (UnresolvedSymlinkArtifactValue.getUnresolvedSymlinkTarget).
    Symlink(PathBuf),
}

/// How [`prepare`] reads an input.
#[derive(Clone, Copy, PartialEq, Eq)]
enum InputKind {
    File,
    Tree,
    SourceDirectory,
    Symlink,
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
    /// A file left in the remote CAS (Build without the Bytes,
    /// crate::remote_outputs): no file is published until a consumer
    /// materializes it. Absent from every other manifest, so those are
    /// byte for byte what they were.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    remote: Option<RemoteRecord>,
}

/// A remote-only output's metadata: its size and when the remote cache's
/// guarantee for it ends (`--experimental_remote_cache_ttl` after it was
/// received; Unix milliseconds).
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(deny_unknown_fields)]
struct RemoteRecord {
    size: u64,
    expires: u64,
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
    /// The published mode of every file that is not OUTPUT_MODE: a file a
    /// symlink led to keeps its target's executable bits (published as its
    /// `0444 | x` blob). Absent from manifests without such files, so those
    /// are byte for byte what they were before tree symlinks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    modes: Vec<(String, u32)>,
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

    /// The digest spelled `text` (64 lowercase hex digits).
    fn from_hex(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() != 64 {
            return None;
        }
        let digit = |byte: u8| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        };
        let mut out = [0u8; 32];
        for (index, pair) in bytes.chunks(2).enumerate() {
            out[index] = digit(pair[0])? << 4 | digit(pair[1])?;
        }
        Some(Self(out))
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

pub(crate) fn validate_relative(path: &Path) -> Result<(), String> {
    // Byte-wise: no empty component (no leading, trailing or repeated
    // separator), no `.` or `..` and no NUL is exactly a relative path whose
    // components are all normal and which `Path::components` spells back byte
    // for byte (so byte equality is path equality for admitted paths).
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty()
        || bytes.contains(&0)
        || bytes
            .split(|&b| b == b'/')
            .any(|c| c.is_empty() || c == b"." || c == b"..")
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
    if let Some(root) = cache {
        let checked = if owned_by(Some(root), path) {
            owned_no_linked_ancestor(path, root, leaf)
        } else {
            source_no_linked_ancestor(path, leaf)
        };
        if let Some(metadata) = checked {
            return Ok(metadata);
        }
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
/// arbitrary code: `modextwd/`, the repository directories `external/`) are
/// never remembered.
/// Other writers racing these checks are outside the module's trust model.
fn verified_directories() -> &'static Shards<FxHashSet<PathBuf>> {
    static MEMO: OnceLock<Shards<FxHashSet<PathBuf>>> = OnceLock::new();
    MEMO.get_or_init(Shards::new)
}

fn directory_verified(path: &Path) -> bool {
    verified_directories()
        .of(path)
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .contains(path)
}

fn remember_directory(path: PathBuf) {
    let mut memo = verified_directories()
        .of(&path)
        .write()
        .unwrap_or_else(|e| e.into_inner());
    if memo.len() >= DIRECTORY_MEMO_LIMIT / SHARDS {
        memo.clear();
    }
    memo.insert(path);
}

/// Shards of a per-process memo keyed by path: a warm build's workers look
/// up its memos hundreds of thousands of times at once, and one lock's
/// shared counter would bounce between all their cores. Each shard keeps
/// the memo's cap divided by [`SHARDS`].
struct Shards<T>(Vec<RwLock<T>>);

const SHARDS: usize = 64;

impl<T: Default> Shards<T> {
    fn new() -> Self {
        Self((0..SHARDS).map(|_| RwLock::default()).collect())
    }

    /// The shard of `path`: FNV-1a of its last 64 bytes (paths share long
    /// prefixes).
    fn of(&self, path: &Path) -> &RwLock<T> {
        let bytes = path.as_os_str().as_bytes();
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in &bytes[bytes.len().saturating_sub(64)..] {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        &self.0[(hash % SHARDS as u64) as usize]
    }

    fn all(&self) -> impl Iterator<Item = &RwLock<T>> {
        self.0.iter()
    }
}

/// Whether `bytes` spell a plain absolute path: a leading `/`, no empty
/// component (`//`), no `.` component and no trailing separator — exactly
/// the absolute paths that `Path::components` rebuilds byte for byte (`..`
/// stays a component). The canonical cache root and planned inputs are
/// spelled so; for two plain paths, `Path::strip_prefix` and the components
/// of its result follow from the bytes ([`plain_rest`]), without parsing
/// components or allocating (the per-input checks of a 6,000-input action).
pub(crate) fn plain(bytes: &[u8]) -> bool {
    bytes.first() == Some(&b'/')
        && !bytes.ends_with(b"/")
        && !bytes.ends_with(b"/.")
        && !bytes.windows(2).any(|pair| pair == b"//")
        && !bytes.windows(3).any(|triple| triple == b"/./")
}

/// For [`plain`] `root` and `path`: the bytes of `path.strip_prefix(root)`
/// when `path` lies strictly below `root` (its components are then the
/// `/`-separated names, none empty), else `None`.
pub(crate) fn plain_rest<'a>(root: &[u8], path: &'a [u8]) -> Option<&'a [u8]> {
    path.strip_prefix(root)?.strip_prefix(b"/")
}

/// Whether `/`-separated names include `..`.
pub(crate) fn has_parent_name(names: &[u8]) -> bool {
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
/// repository rules run arbitrary code in `modextwd/` and in the repository
/// directories `external/<name>` (crate::repository_slots: a link to a tree,
/// or mutable), and downloads, locks, manifests and specification pointers
/// are no trees.
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
    if parent != root && !directory_verified(parent) && !verify_directories(parent, root) {
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
    if metadata.is_dir() {
        remember_directory(path.to_path_buf());
    }
    Some(metadata)
}

/// Directories outside the cache (the workspace's source directories,
/// local repositories) found to be real directories by lstat, each together
/// with every directory above it, in this process: like Bazel's per-build
/// file system values (FileValue of each ancestor, computed once per
/// build), each is inspected once per command instead of once per action
/// input (a warm TypeScript build reads its sources ~90k times through ten
/// ancestors each). A directory replaced by a symlink while the command
/// runs, after this first look, is not re-detected by the command; inputs
/// are still read (and staged) by their own lstat and digest.
fn source_directories() -> &'static RwLock<FxHashSet<PathBuf>> {
    static MEMO: OnceLock<RwLock<FxHashSet<PathBuf>>> = OnceLock::new();
    MEMO.get_or_init(|| RwLock::new(FxHashSet::default()))
}

/// The memoized [`no_linked_ancestor`] of a [`plain`] path outside the cache
/// root ([`source_directories`]); the leaf is always inspected (or `leaf`,
/// the caller's lstat of it, is used). `None` when the path is not plain or
/// anything fails, so that the full walk produces the result (and the exact
/// error) it always did.
fn source_no_linked_ancestor(path: &Path, leaf: Option<fs::Metadata>) -> Option<fs::Metadata> {
    let bytes = path.as_os_str().as_bytes();
    if !plain(bytes) || has_parent_name(bytes) {
        return None;
    }
    let parent = path.parent()?;
    let verified = |directory: &Path| {
        source_directories()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(directory)
    };
    if !verified(parent) {
        // The directories from the top, skipping those already verified.
        let mut prefix = PathBuf::from("/");
        for component in parent.strip_prefix("/").ok()?.components() {
            prefix.push(component);
            if verified(&prefix) {
                continue;
            }
            // lstat: a symlink is never a directory here.
            if !fs::symlink_metadata(&prefix).ok()?.is_dir() {
                return None;
            }
            let mut memo = source_directories()
                .write()
                .unwrap_or_else(|e| e.into_inner());
            if memo.len() >= DIRECTORY_MEMO_LIMIT {
                memo.clear();
            }
            memo.insert(prefix.clone());
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

/// Verify (and remember) every directory from `directory` up to the cache
/// root `root` that is not verified yet: the deepest verified one is opened
/// (`O_PATH`), and each directory below it relative to its parent's
/// descriptor, never through a symlink (`O_NOFOLLOW | O_DIRECTORY`), instead
/// of an lstat of each one's absolute path. Each action entry's directories
/// are new to a warm build, and on overlay filesystems absolute-path lookups
/// serialize across threads (crate::dir_stamps). Any failure leaves the
/// per-directory walk to decide (and to report).
fn verify_directories(directory: &Path, root: &Path) -> bool {
    let Ok(below) = directory.strip_prefix(root) else {
        return false;
    };
    let names: Vec<&OsStr> = below.iter().collect();
    // The deepest verified directory (the root at the least).
    let mut start = names.len();
    let mut base = directory.to_path_buf();
    while start > 0 {
        base.pop();
        start -= 1;
        if base == root || directory_verified(&base) {
            break;
        }
    }
    let Ok(mut parent) = crate::dir_stamps::open_directory(None, &base) else {
        return false;
    };
    for name in &names[start..] {
        let Ok(opened) = crate::dir_stamps::open_directory(Some(&parent), Path::new(name)) else {
            return false;
        };
        base.push(name);
        remember_directory(base.clone());
        parent = opened;
    }
    true
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
/// modification time and size). The executor's own files (`owned`: store
/// blobs, published outputs and execution-root outputs, below the cache
/// root) leave out the change time: every link() to a blob moves it
/// (staging links blobs constantly), while their bytes stay the same. They
/// keep their birth time in its place (`(0, 0)` where the filesystem
/// reports none): link() leaves it alone, and a new file gets a new one —
/// a later one only once the state is settled ([`settled`]). The kind is
/// part of the key, so a birth time never matches a change time. (The
/// persistent memo keys repository tree files by their change time:
/// [`hash_file_from`].)
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct FileKey {
    owned: bool,
    device: u64,
    inode: u64,
    size: u64,
    mtime: (i64, i64),
    /// The change time, or an owned file's birth time.
    ctime: (i64, i64),
}

/// A file's birth time as (seconds, nanoseconds), where the filesystem
/// reports one.
fn birth_time(metadata: &fs::Metadata) -> Option<(i64, i64)> {
    let birth = metadata
        .created()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?;
    Some((
        i64::try_from(birth.as_secs()).ok()?,
        i64::from(birth.subsec_nanos()),
    ))
}

fn file_key(metadata: &fs::Metadata, owned: bool) -> FileKey {
    FileKey {
        owned,
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.size(),
        mtime: (metadata.mtime(), metadata.mtime_nsec()),
        ctime: if owned {
            birth_time(metadata).unwrap_or((0, 0))
        } else {
            (metadata.ctime(), metadata.ctime_nsec())
        },
    }
}

/// The per-process digest memo. It answers only for a file state this
/// process observed (or produced), never for another file that merely
/// shows the same stat: a file deleted and replaced by one of the same size
/// inside one timestamp tick usually passes its inode number on at once
/// (always on overlayfs), with identical timestamps, birth time included.
#[derive(Default)]
struct DigestMemo {
    /// States read when they were settled ([`settled`]): no file created
    /// or changed after that read can show such a state again, so it names
    /// one inode with one content for good and answers for every path of
    /// that inode (a published output and its blob).
    settled: HashMap<FileKey, Sha256Digest>,
    /// By path, racy states whose content this process established on that
    /// very path ([`remember_produced`]): a store blob it validated or
    /// created (the name is the content's digest; this process removes an
    /// invalid blob and only ever creates valid ones) and a published output
    /// it linked to such a blob. They answer for that path only, and
    /// whenever this process removes or replaces the path it forgets them
    /// ([`forget_produced`], [`forget_published`]).
    produced: HashMap<PathBuf, (FileKey, Sha256Digest)>,
    /// The produced paths below each action entry (`actions/<key>`).
    entries: HashMap<PathBuf, Vec<PathBuf>>,
}

/// The digest memo; lookups (most calls of a warm build, from every worker
/// at once) share it ([`digest_memo_read`]).
fn digest_memo_lock() -> &'static RwLock<DigestMemo> {
    static MEMO: OnceLock<RwLock<DigestMemo>> = OnceLock::new();
    MEMO.get_or_init(|| RwLock::new(DigestMemo::default()))
}

fn digest_memo() -> std::sync::RwLockWriteGuard<'static, DigestMemo> {
    digest_memo_lock()
        .write()
        .unwrap_or_else(|e| e.into_inner())
}

fn digest_memo_read() -> std::sync::RwLockReadGuard<'static, DigestMemo> {
    digest_memo_lock().read().unwrap_or_else(|e| e.into_inner())
}

/// The digest this process knows for `path` in the state `key`, and
/// whether it knows it from a settled state.
fn memoized(path: &Path, key: &FileKey) -> Option<(Sha256Digest, bool)> {
    let memo = digest_memo_read();
    if let Some(digest) = memo.settled.get(key) {
        return Some((*digest, true));
    }
    memo.produced
        .get(path)
        .filter(|(state, _)| state == key)
        .map(|(_, digest)| (*digest, false))
}

fn memoized_digest(path: &Path, key: &FileKey) -> Option<Sha256Digest> {
    memoized(path, key).map(|(digest, _)| digest)
}

/// Remember the digest of a settled state ([`DigestMemo::settled`]).
fn remember_settled(key: FileKey, digest: Sha256Digest) {
    let mut memo = digest_memo();
    if memo.settled.len() >= DIGEST_MEMO_LIMIT {
        memo.settled.clear();
    }
    memo.settled.insert(key, digest);
}

/// Remember that `path` — a store blob this process validated or created,
/// or (`entry`: its action entry) a published output it linked to such a
/// blob — has `digest` in the state `key` ([`DigestMemo::produced`]).
fn remember_produced(path: &Path, key: FileKey, digest: Sha256Digest, entry: Option<&Path>) {
    let mut memo = digest_memo();
    if memo.produced.len() >= DIGEST_MEMO_LIMIT && !memo.produced.contains_key(path) {
        memo.produced.clear();
        memo.entries.clear();
    }
    if memo
        .produced
        .insert(path.to_path_buf(), (key, digest))
        .is_none()
        && let Some(entry) = entry
    {
        memo.entries
            .entry(entry.to_path_buf())
            .or_default()
            .push(path.to_path_buf());
    }
}

/// This process is about to remove or replace `path` (an invalid store
/// blob): forget what it knew of the file there, also its not yet saved
/// persistent record and its file memo entry.
fn forget_produced(path: &Path) {
    digest_memo().produced.remove(path);
    if let Some(table) = digest_table() {
        table.forget(path);
    }
    file_memo()
        .of(path)
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .remove(path);
}

/// This process is about to remove the action entry `entry`
/// (`actions/<key>`, to publish it again): forget the published outputs it
/// produced below it, also their not yet saved persistent records.
fn forget_published(entry: &Path) {
    let paths = {
        let mut memo = digest_memo();
        let paths = memo.entries.remove(entry).unwrap_or_default();
        for path in &paths {
            memo.produced.remove(path);
        }
        paths
    };
    if let Some(table) = digest_table() {
        for path in &paths {
            table.forget(path);
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Test hook: the persistent digest memo this thread's executor uses
    /// instead of the installed one ([`digest_table`]).
    static TEST_TABLE: std::cell::Cell<Option<&'static crate::digest_memo::Table>> =
        const { std::cell::Cell::new(None) };
}

/// The persistent digest memo of this command (crate::digest_memo::installed).
fn digest_table() -> Option<&'static crate::digest_memo::Table> {
    #[cfg(test)]
    if let Some(table) = TEST_TABLE.with(std::cell::Cell::get) {
        return Some(table);
    }
    crate::digest_memo::installed()
}

/// Whether a file state was settled at `start`: its modification time and
/// its change time — for one of the executor's own files (`owned`) its
/// birth time instead, or where the filesystem reports none its change
/// time — at least crate::digest_memo::RACY_MARGIN older ("racy git",
/// crate::digest_memo::settled). A file changed or created after `start`
/// necessarily carries a later timestamp, so no other content can ever show
/// a settled state again, not even after its inode number is reused. An
/// owned file is not judged by its change time, which every link() to it
/// moves while its bytes stay the same; and a modification time alone never
/// settles a file (`touch -d`, `cp -p` and archive extraction give new
/// files old ones).
fn settled(metadata: &fs::Metadata, start: SystemTime, owned: bool) -> bool {
    crate::digest_memo::settled(metadata, start, owned)
}

/// Streamed SHA-256 of a regular file. The file must not change while it is
/// read. `publishing`: an execution-root output being published, hashed
/// without being remembered by its path (see [`hash_file_from`]).
#[cfg(test)]
fn hash_file(
    path: &Path,
    owned: bool,
    publishing: bool,
) -> Result<(Sha256Digest, fs::Metadata), String> {
    let before = fs::symlink_metadata(path).map_err(|e| io_error("cannot inspect", path, e))?;
    hash_file_from(path, before, owned, publishing)
}

/// [`hash_file`] with `before`, the caller's lstat of `path` (taken just
/// before: the file must not change from then on). The digest comes from
/// the per-process memo ([`DigestMemo`]: a settled state of the inode on
/// any path, or what this process produced at this very path), else from
/// the persistent memo of this path in exactly this state (crate::digest_memo,
/// when the command installed one), else from reading the file; the state
/// read is remembered when it was settled ([`settled`]), in the persistent
/// memo as well for paths that recur across processes (never an execution
/// root).
///
/// `publishing`: `path` is an execution-root output being published. Its
/// path goes away with the execution root, so neither memo is consulted by
/// it or remembers it; the caller passes the returned state on to the blob
/// it links ([`ensure_blob_hashed`]), and the published path is remembered
/// once it links that blob ([`remember_published`]).
///
/// The persistent memo keys a file of a repository tree store by its change
/// time, unlike the per-process memo (crate::digest_memo::Area::Tree): this
/// command verified the trees it materialized, but a published symlink
/// output can still point into a tree identity that no command verifies any
/// more, where an in-place edit can keep the size and restore the mtime.
/// Such a file enters the persistent memo only from a read of a state
/// settled by both times and left unchanged by the read — never as trusted
/// and never from a per-process entry, which outlives such an edit made
/// while the command runs.
fn hash_file_from(
    path: &Path,
    before: fs::Metadata,
    owned: bool,
    publishing: bool,
) -> Result<(Sha256Digest, fs::Metadata), String> {
    let key = file_key(&before, owned);
    // (table, its key, whether `path` is a repository tree file).
    let table = digest_table().filter(|_| !publishing).and_then(|table| {
        let tree = owned && table.area(path) == crate::digest_memo::Area::Tree;
        Some((
            table,
            crate::digest_memo::Key::of(&before, owned && !tree)?,
            tree,
        ))
    });
    if let Some((digest, settled_state)) = memoized(path, &key) {
        // A settled state this process read, maybe through another path (a
        // published output and its blob): record it under this one too
        // (trusted: derived knowledge, which never causes a save by itself).
        // Not a tree file (see above). A produced path was recorded when it
        // was produced.
        if settled_state && let Some((table, table_key, false)) = &table {
            table.remember(path, *table_key, digest.0, true);
        }
        return Ok((digest, before));
    }
    if let Some((table, table_key, _)) = &table
        && let Some(sha256) = table.lookup(path, table_key)
    {
        // A record of this path in exactly this state. It answers for other
        // paths of the inode only once the state is settled.
        let digest = Sha256Digest(sha256);
        if settled(&before, SystemTime::now(), owned) {
            remember_settled(key, digest);
        }
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
    let settled_state = settled(&after, start, owned);
    if settled_state {
        remember_settled(key, digest);
    }
    match table {
        // A tree file: settled by its change time as well, which the read
        // left unchanged; never trusted.
        Some((table, table_key, true)) => {
            if settled(&after, start, false)
                && crate::digest_memo::Key::of(&after, false) == Some(table_key)
            {
                table.remember(path, table_key, digest.0, false);
            }
        }
        Some((table, table_key, false)) if settled_state => {
            table.remember(path, table_key, digest.0, false);
        }
        _ => {}
    }
    Ok((digest, before))
}

/// The SHA-256 of source file `path` whose (followed) metadata is
/// `metadata`, through the digest memos like an action input's
/// (crate::affected's target digests).
pub fn source_file_digest(path: &Path, metadata: fs::Metadata) -> Result<[u8; 32], String> {
    hash_file_from(path, metadata, false, false).map(|(digest, _)| digest.0)
}

/// After publication: `path`, a published output file below the action
/// entry `entry` with the content of `input` (its digest and executable
/// bits), is remembered ([`remember_owned`]) — so that its consumers in
/// this process and the next command's cache check do not read it again —
/// when it is the inode of that content's store blob in the state this
/// process validated or created the blob in: the blob is the proof of its
/// content, and keeps the inode number from being reused. Otherwise (a
/// filesystem without hardlinks) it is read when it is used.
fn remember_published(cas: &Path, entry: &Path, path: &Path, input: &Input) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_file()
        && memoized_digest(&blob_path(cas, input), &file_key(&metadata, true)) == Some(input.digest)
    {
        remember_owned(path, &metadata, input.digest, Some(entry));
    }
}

/// Remember that the cache-owned file `path` — a store blob this process
/// validated or created, or (`entry`: its action entry) a published output
/// linked to such a blob — has `digest` in the state `metadata` (its lstat,
/// taken after this process established the content): a settled state for
/// every path of its inode, a racy one by this path only
/// ([`DigestMemo::produced`]); and in the persistent memo (trusted: derived
/// from what this process knows; it never causes a save by itself).
fn remember_owned(
    path: &Path,
    metadata: &fs::Metadata,
    digest: Sha256Digest,
    entry: Option<&Path>,
) {
    let key = file_key(metadata, true);
    if settled(metadata, SystemTime::now(), true) {
        remember_settled(key, digest);
    } else {
        remember_produced(path, key, digest, entry);
    }
    if let Some(table) = digest_table()
        && let Some(table_key) = crate::digest_memo::Key::of(metadata, true)
    {
        table.remember(path, table_key, digest.0, true);
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
                    // A directory this process verified is not inspected
                    // again (owned_no_linked_ancestor remembers it).
                    if !(owned_area(parent, root) && directory_verified(parent)) {
                        no_linked_ancestor(parent, Some(root))?;
                    }
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
fn file_memo() -> &'static Shards<FxHashMap<PathBuf, StoreFile>> {
    static MEMO: OnceLock<Shards<FxHashMap<PathBuf, StoreFile>>> = OnceLock::new();
    MEMO.get_or_init(Shards::new)
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
        .of(path)
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

/// The lock that [`read_input`] holds while it reads the store file `path`
/// for the file memo (single flight). Dropped once the file is remembered:
/// the map holds only the files being read.
fn store_file_flight(path: &Path) -> FileFlight {
    static FLIGHTS: OnceLock<Mutex<HashMap<PathBuf, std::sync::Weak<Mutex<()>>>>> = OnceLock::new();
    let mut flights = FLIGHTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(flight) = flights.get(path).and_then(std::sync::Weak::upgrade) {
        return flight;
    }
    flights.retain(|_, flight| flight.strong_count() > 0);
    let flight = Arc::new(Mutex::new(()));
    flights.insert(path.to_path_buf(), Arc::downgrade(&flight));
    flight
}

type FileFlight = Arc<Mutex<()>>;

fn remember_file(input: &Input) {
    let mut memo = file_memo()
        .of(&input.path)
        .write()
        .unwrap_or_else(|e| e.into_inner());
    if memo.len() >= FILE_MEMO_LIMIT / SHARDS {
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

/// Whether `path` is a file of a published repository tree below the
/// canonical cache root (a [`store_file`] that is no `cas/` blob), spelled
/// exactly as its normal components.
fn repository_tree_file(root: &Path, path: &Path) -> bool {
    let tree_file = |rest: &[u8]| repository_tree_rest(rest).is_some_and(|below| !below.is_empty());
    let (root_bytes, bytes) = (root.as_os_str().as_bytes(), path.as_os_str().as_bytes());
    if !plain(root_bytes) {
        return relative_names(path, root).is_some_and(|names| tree_file(&names))
            && path.components().collect::<PathBuf>().as_os_str() == path.as_os_str();
    }
    plain(bytes)
        && plain_rest(root_bytes, bytes)
            .is_some_and(|rest| !has_parent_name(rest) && tree_file(rest))
}

/// Digest and mode of a regular file; `root` (the canonical cache root)
/// enables following published symlink outputs when `follow` is set. A file
/// of the cache's immutable stores is read once per process ([`file_memo`]).
fn read_input(path: &Path, root: Option<&Path>, follow: bool) -> Result<Input, String> {
    if let Some(input) = remote_only_input(path) {
        return Ok(input);
    }
    let (path, leaf) = if follow {
        resolve_published(path, root)?
    } else {
        (path.to_path_buf(), None)
    };
    let stored = store_file(root, &path);
    if stored && let Some(input) = memoized_file(&path) {
        return Ok(input);
    }
    // One reader per store file at a time: the actions that start together
    // read the same files (a toolchain, a package's sources), and every
    // other reader takes the first one's result from the memo.
    let flight = stored.then(|| store_file_flight(&path));
    let _reading = flight
        .as_ref()
        .map(|flight| flight.lock().unwrap_or_else(|e| e.into_inner()));
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

/// A remote-only published output (crate::remote_outputs) as an input:
/// its digest and size, published mode, never read.
fn remote_only_input(path: &Path) -> Option<Input> {
    let file = crate::remote_outputs::get(path)?;
    Some(Input {
        path: path.to_path_buf(),
        digest: Sha256Digest::from_hex(&file.hash)?,
        size: file.size,
        executable: 0o111,
        mode: OUTPUT_MODE,
    })
}

/// Materialize the remote-only output published at `path`
/// (RemoteActionInputFetcher before a local consumer runs): its blob is
/// downloaded (verified) into the store and linked at its published path,
/// where it then is an ordinary published output. A blob the remote CAS
/// lost invalidates its producer's cache entry and fails the consumer
/// with Bazel's lost-input text; the command is retried
/// (`--experimental_remote_cache_eviction_retries`).
fn materialize(cas: &Path, root: &Path, path: &Path) -> Result<(), String> {
    if crate::remote_outputs::get(path).is_none() {
        return Ok(());
    }
    static STRIPES: [Mutex<()>; 64] = [const { Mutex::new(()) }; 64];
    let stripe = usize::try_from(crate::digest_memo::path_hash(path)).unwrap_or(0) % 64;
    let _single = STRIPES[stripe].lock().unwrap_or_else(|e| e.into_inner());
    let Some(file) = crate::remote_outputs::get(path) else {
        return Ok(());
    };
    let exec = action_entry(root, path).and_then(|entry| {
        path.strip_prefix(entry.join("outputs"))
            .ok()
            .map(Path::to_path_buf)
    });
    if fs::symlink_metadata(path).is_ok() {
        crate::remote_outputs::materialized(path, None);
        return Ok(());
    }
    let shown = exec.as_deref().unwrap_or(path).display().to_string();
    let remote = crate::remote_cache::active().ok_or_else(|| {
        format!("remote-only output {shown} needs the remote cache, which is not configured")
    })?;
    let digest = crate::reapi::Digest {
        hash: file.hash.clone(),
        size: file.size,
    };
    let bytes = match remote.fetch_blob(&digest) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            // ActionRewindStrategy's lost inputs: the producer reruns on
            // the next attempt.
            lost_remote_output(path);
            return Err(format!(
                "Lost inputs no longer available remotely: {shown} ({digest})"
            ));
        }
        Err(error) => {
            crate::remote_cache::note_remote_failure();
            return Err(format!("Failed to fetch remote input {shown}: {error}"));
        }
    };
    let blob = store_bytes(cas, &bytes, 0o111)?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("invalid output path {}", path.display()))?;
    ensure_directory(parent)?;
    let replacement = temporary(parent, "remote");
    if hard_link(&blob, &replacement).is_err() {
        copy_verified(&blob, &replacement, digest_of_hex(&file.hash)?)?;
        set_mode(&replacement, OUTPUT_MODE)?;
    }
    fs::rename(&replacement, path).map_err(|e| io_error("cannot publish", path, e))?;
    crate::remote_outputs::materialized(path, exec.as_deref());
    Ok(())
}

/// The remote-only output published at `path` is gone from the remote CAS:
/// its producer's cache entry is invalidated (the producer reruns on the
/// next attempt) and the command is marked for a retry.
pub(crate) fn lost_remote_output(path: &Path) {
    let hash = crate::remote_outputs::get(path).map(|file| file.hash);
    crate::remote_outputs::note_lost(hash.as_deref().unwrap_or(""));
    let entry = crate::remote_outputs::entry_of(path).map(Path::to_path_buf);
    if let Some(entry) = entry {
        let _ = fs::remove_file(entry.join("manifest.json"));
        forget_outputs(&entry);
        forget_published(&entry);
        crate::remote_outputs::forget_entry(&entry);
    }
}

fn digest_of_hex(text: &str) -> Result<Sha256Digest, String> {
    Sha256Digest::from_hex(text).ok_or_else(|| format!("invalid digest '{text}'"))
}

/// read_input for a path already checked to be a regular file.
#[cfg(test)]
fn read_regular(path: &Path, owned: bool, publishing: bool) -> Result<Input, String> {
    let metadata = fs::symlink_metadata(path).map_err(|e| io_error("cannot inspect", path, e))?;
    read_regular_from(path, metadata, owned, publishing)
}

/// [`read_regular`] with `metadata`, the caller's lstat of `path` (taken
/// just before).
fn read_regular_from(
    path: &Path,
    metadata: fs::Metadata,
    owned: bool,
    publishing: bool,
) -> Result<Input, String> {
    read_regular_state(path, metadata, owned, publishing).map(|(input, _)| input)
}

/// [`read_regular_from`], with the state the file was read in (for an
/// output being published: [`ensure_blob_hashed`]).
fn read_regular_state(
    path: &Path,
    metadata: fs::Metadata,
    owned: bool,
    publishing: bool,
) -> Result<(Input, FileKey), String> {
    let (digest, metadata) = hash_file_from(path, metadata, owned, publishing)?;
    if !metadata.is_file() {
        return Err(format!("expected regular file: '{}'", path.display()));
    }
    let input = Input {
        path: path.to_path_buf(),
        digest,
        size: metadata.size(),
        executable: metadata.permissions().mode() & 0o111,
        mode: metadata.permissions().mode() & 0o7777,
    };
    Ok((input, file_key(&metadata, owned)))
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

/// [`try_map_ordered`] that maps every item, also after a failure.
fn try_map_all<T, R>(
    items: &[T],
    map: impl Fn(&T) -> Result<R, String> + Sync,
) -> Vec<Result<R, String>>
where
    T: Sync,
    R: Send,
{
    try_map_in(helper_budget(), items, map, false)
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
    try_map_in(budget, items, map, true)
}

/// [`try_map_ordered_in`]; `stop` at the first failure when mapping on the
/// calling thread alone (otherwise every item is mapped).
fn try_map_in<T, R>(
    budget: &HelperBudget,
    items: &[T],
    map: impl Fn(&T) -> Result<R, String> + Sync,
    stop: bool,
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
            if failed && stop {
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

/// A regular file [`read_tree`] found; it is read after the walk.
struct WalkedFile {
    /// Its path relative to the tree root.
    child: PathBuf,
    /// Where it is read: its own path, or the canonical path of the file a
    /// symlink led to.
    path: PathBuf,
    /// Its lstat, when nothing changed the file since (no chmod).
    metadata: Option<fs::Metadata>,
    owned: bool,
    /// An output tree's own normalized file: an output being published,
    /// hashed without being remembered by its path ([`hash_file_from`]).
    publishing: bool,
}

fn identity(metadata: &fs::Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

/// Whether a link at `child` (relative to a tree root) with the relative
/// `target` names a path outside the tree, lexically.
fn leaves_tree(child: &Path, target: &Path) -> bool {
    if target.is_absolute() {
        return false;
    }
    let mut depth = child
        .parent()
        .map_or(0, |parent| parent.components().count());
    for component in target.components() {
        match component {
            Component::ParentDir if depth == 0 => return true,
            Component::ParentDir => depth -= 1,
            Component::Normal(_) => depth += 1,
            _ => {}
        }
    }
    false
}

/// Walk a tree, the names of each directory in byte order. `mode` decides
/// about symlinks ([`TreeRead`]): a published tree has none (an explicit
/// error); anywhere else a symlink is followed as TreeArtifactValue.visitTree
/// (skyframe/TreeArtifactValue.java:548-592) follows it: a link to a file is
/// that file at the link's path, a link to a directory is walked at the
/// link's path; a dangling link (`child X is a dangling symbolic link`), a
/// link to anything else and a special file (`child X has an unsupported
/// type`) and a link that cannot be resolved (ELOOP) fail. A directory that
/// a link leads back into one of the directories it lies in is reported as
/// ELOOP at once (genuine recurses through the cycle until the kernel's
/// limit of 40 links per lookup stops it). An output tree's entries that no
/// link led to are made OUTPUT_MODE before they are read (completed tree
/// outputs, ActionOutputMetadataStore.java:263-282); such files are outputs
/// being published (see [`hash_file_from`]); a file a link led to is read
/// where it lies, like any input. The files are read after the walk
/// ([`try_map_ordered`]); the first failure in walk order is reported,
/// exactly as if each file were read when the walk reached it.
fn read_tree(root: &Path, cache: Option<&Path>, mode: TreeRead<'_>) -> Result<Tree, String> {
    read_walked(walk_tree(root, cache, mode)?).map(|(tree, _)| tree)
}

/// A walked tree ([`walk_tree`]), whose files [`read_walked`] reads.
struct WalkedTree {
    directories: BTreeSet<PathBuf>,
    links: BTreeSet<PathBuf>,
    files: Vec<WalkedFile>,
    /// The walk's failure, reported after the files found before it.
    failure: Option<String>,
}

/// [`read_tree`]'s files, in walk order, then its walk failure; with the
/// state each output being published was read in (its normalized files:
/// [`ensure_blob_hashed`]), by its name.
fn read_walked(walked: WalkedTree) -> Result<(Tree, BTreeMap<PathBuf, FileKey>), String> {
    let inputs = try_map_ordered(&walked.files, |file| {
        let metadata = match &file.metadata {
            Some(metadata) => metadata.clone(),
            None => fs::symlink_metadata(&file.path)
                .map_err(|e| io_error("cannot inspect", &file.path, e))?,
        };
        read_regular_state(&file.path, metadata, file.owned, file.publishing)
    });
    let mut tree = Tree {
        files: BTreeMap::new(),
        directories: walked.directories,
        links: walked.links,
    };
    let mut states = BTreeMap::new();
    for (file, read) in walked.files.into_iter().zip(inputs) {
        let (input, state) = read?;
        if file.publishing {
            states.insert(file.child.clone(), state);
        }
        tree.files.insert(file.child, input);
    }
    match walked.failure {
        Some(failure) => Err(failure),
        None => Ok((tree, states)),
    }
}

/// [`read_tree`]'s walk: the directories, links and files to read (an
/// output tree's entries are normalized here), or the root's failure.
fn walk_tree(root: &Path, cache: Option<&Path>, mode: TreeRead<'_>) -> Result<WalkedTree, String> {
    #[cfg(test)]
    TREE_WALKS.with(|walks| walks.set(walks.get() + 1));
    let root_metadata = real_directory(root, cache)?;
    let owned = owned_by(cache, root);
    let normalize = matches!(mode, TreeRead::Output(_));
    // A validation failure names the tree like Bazel's
    // reportOutputTreeArtifactErrors (SkyframeActionExecutor.java:1554-1567).
    let invalid = |message: String| match mode {
        TreeRead::Output(output) => format!(
            "error while validating output tree artifact {}: {message}",
            output.pretty
        ),
        _ => format!("tree '{}': {message}", root.display()),
    };
    let too_many_links = |path: &Path| {
        invalid(format!(
            "{} (Too many levels of symbolic links)",
            path.display()
        ))
    };
    // What the symlink at `path` (`child` below the root) resolves to.
    let follow = |path: &Path, child: &Path| -> Result<fs::Metadata, String> {
        let dangling = |hint: String| {
            invalid(format!(
                "child {} is a dangling symbolic link{hint}",
                child.display()
            ))
        };
        let target = match mode {
            TreeRead::Output(_) => {
                Some(fs::read_link(path).map_err(|e| io_error("cannot read link", path, e))?)
            }
            _ => None,
        };
        if let (TreeRead::Output(output), Some(target)) = (mode, &target)
            && target.is_absolute()
            && target.starts_with(output.work)
            && !output
                .execroot
                .is_some_and(|execroot| target.starts_with(execroot))
        {
            return Err(dangling(
                "; its absolute target lies in the action's sandbox or temporary directory, which are gone once the action has run"
                    .to_owned(),
            ));
        }
        match fs::metadata(path) {
            Ok(metadata) => Ok(metadata),
            Err(e)
                if e.kind() == io::ErrorKind::NotFound
                    || e.raw_os_error() == Some(rustix::io::Errno::NOTDIR.raw_os_error()) =>
            {
                Err(dangling(match &target {
                    Some(target) if leaves_tree(child, target) => format!(
                        "; its target '{}' lies outside the tree, and a native execution root holds only the action's inputs and outputs",
                        target.display()
                    ),
                    _ => String::new(),
                }))
            }
            Err(e) if e.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) => {
                Err(too_many_links(path))
            }
            Err(e) => Err(io_error("cannot inspect", path, e)),
        }
    };
    let mut directories = BTreeSet::new();
    let mut links = BTreeSet::new();
    let mut files: Vec<WalkedFile> = Vec::new();
    let mut walk = || -> Result<(), String> {
        // Directories to list: the relative path, whether a link led there,
        // and the identities of the directories it lies in (itself
        // included).
        let mut pending = vec![(PathBuf::new(), false, vec![identity(&root_metadata)])];
        while let Some((relative, linked, ancestors)) = pending.pop() {
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
                let (metadata, linked) = if !metadata.file_type().is_symlink() {
                    (metadata, linked)
                } else if matches!(mode, TreeRead::Published) {
                    return Err(format!(
                        "published tree artifact entry '{}' is a symlink",
                        path.display()
                    ));
                } else {
                    let resolved = follow(&path, &child)?;
                    links.insert(child.clone());
                    (resolved, true)
                };
                if metadata.is_dir() {
                    let directory = identity(&metadata);
                    if ancestors.contains(&directory) {
                        return Err(too_many_links(&path));
                    }
                    let mut inner = ancestors.clone();
                    inner.push(directory);
                    directories.insert(child.clone());
                    pending.push((child, linked, inner));
                } else if metadata.is_file() && linked {
                    // Read where the links lead, never chmod'ed: it can lie
                    // outside the tree (`chmod && !traversedSymlink`).
                    let target = fs::canonicalize(&path)
                        .map_err(|e| io_error("cannot resolve", &path, e))?;
                    files.push(WalkedFile {
                        child,
                        owned: owned_by(cache, &target),
                        path: target,
                        metadata: None,
                        publishing: false,
                    });
                } else if metadata.is_file() {
                    if normalize {
                        set_mode(&path, OUTPUT_MODE)?;
                    }
                    files.push(WalkedFile {
                        child,
                        path,
                        metadata: (!normalize).then_some(metadata),
                        owned,
                        publishing: normalize,
                    });
                } else {
                    return Err(invalid(format!(
                        "child {} has an unsupported type",
                        child.display()
                    )));
                }
            }
        }
        Ok(())
    };
    let failure = walk().err();
    Ok(WalkedTree {
        directories,
        links,
        files,
        failure,
    })
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

fn tree_memo() -> &'static RwLock<TreeMemo> {
    static MEMO: OnceLock<RwLock<TreeMemo>> = OnceLock::new();
    MEMO.get_or_init(|| RwLock::new(TreeMemo::default()))
}

fn tree_entries(tree: &Tree) -> usize {
    tree.files.len() + tree.directories.len()
}

fn memoized_tree(path: &Path) -> Option<Arc<Tree>> {
    tree_memo()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .trees
        .get(path)
        .cloned()
}

fn remember_tree(path: PathBuf, tree: Arc<Tree>) {
    let mut memo = tree_memo().write().unwrap_or_else(|e| e.into_inner());
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
    let mut memo = tree_memo().write().unwrap_or_else(|e| e.into_inner());
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

/// What a consumer reads of a published output ([`read_staged`]): a file
/// (through the published symlinks that lead to it) or an unresolved
/// symlink's link text.
#[derive(Clone)]
enum PublishedValue {
    /// The file and, when the published path is that file itself (no
    /// symlink output), its device and inode.
    File(Input, Option<(u64, u64)>),
    Link(PathBuf),
}

/// What the output tree links for a published output (crate::output_tree):
/// the published file itself (a hardlink of this device and inode), or a
/// symlink to its path (a tree, a symlink output).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishedKind {
    File { device: u64, inode: u64 },
    Link,
}

/// [`PublishedKind`] of `path` when this process validated it in a cache
/// check (the output and tree memos), without inspecting it again; `None`
/// when it does not know.
pub fn published_kind(path: &Path) -> Option<PublishedKind> {
    match memoized_output(path) {
        Some(PublishedValue::File(_, Some((device, inode)))) => {
            return Some(PublishedKind::File { device, inode });
        }
        Some(PublishedValue::File(input, None)) if input.path != path => {
            return Some(PublishedKind::Link);
        }
        Some(PublishedValue::Link(_)) => return Some(PublishedKind::Link),
        _ => {}
    }
    memoized_tree(path).map(|_| PublishedKind::Link)
}

/// Published outputs (other than trees: [`TreeMemo`]) by path, as their
/// producer's valid cache entry showed them ([`cache_valid`]), read once
/// per process and shared by every consumer like Bazel's per-build output
/// metadata, instead of being inspected again by each consuming action (a
/// TypeScript build's actions read hundreds of thousands of published
/// files and `node_modules` links). The trust is the tree memo's: a
/// published output is read-only and replaced only by its producer's
/// republication, which drops it first ([`forget_outputs`]).
struct OutputMemo {
    values: Shards<FxHashMap<PathBuf, PublishedValue>>,
    /// The memoized paths that depend on each action entry
    /// (`actions/<key>`): its own outputs, and the published symlinks of
    /// other entries that resolve into it; and how many values there are.
    entries: Mutex<(HashMap<PathBuf, Vec<PathBuf>>, usize)>,
}

fn output_memo() -> &'static OutputMemo {
    static MEMO: OnceLock<OutputMemo> = OnceLock::new();
    MEMO.get_or_init(|| OutputMemo {
        values: Shards::new(),
        entries: Mutex::default(),
    })
}

fn memoized_output(path: &Path) -> Option<PublishedValue> {
    output_memo()
        .values
        .of(path)
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(path)
        .cloned()
}

/// The action entry (`<root>/actions/<key>`) of a published path.
fn action_entry(root: &Path, path: &Path) -> Option<PathBuf> {
    let key = path
        .strip_prefix(root.join("actions"))
        .ok()?
        .components()
        .next()?;
    Some(root.join("actions").join(key))
}

/// Remember the published outputs of one valid cache entry: each path, its
/// value, and the entries it depends on.
fn remember_outputs(root: &Path, values: Vec<(PathBuf, PublishedValue)>) {
    if values.is_empty() {
        return;
    }
    let memo = output_memo();
    let mut entries = memo.entries.lock().unwrap_or_else(|e| e.into_inner());
    if entries.1 + values.len() > OUTPUT_MEMO_LIMIT {
        for shard in memo.values.all() {
            shard.write().unwrap_or_else(|e| e.into_inner()).clear();
        }
        *entries = Default::default();
    }
    for (path, value) in values {
        let mut dependencies = vec![action_entry(root, &path)];
        if let PublishedValue::File(input, _) = &value
            && input.path != path
        {
            dependencies.push(action_entry(root, &input.path));
        }
        for entry in dependencies.into_iter().flatten() {
            entries.0.entry(entry).or_default().push(path.clone());
        }
        let previous = memo
            .values
            .of(&path)
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(path, value);
        if previous.is_none() {
            entries.1 += 1;
        }
    }
}

/// This process is about to remove the action entry `entry` (to publish it
/// again): forget its outputs, and the symlinks that resolve into it.
fn forget_outputs(entry: &Path) {
    let memo = output_memo();
    let mut entries = memo.entries.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(paths) = entries.0.remove(entry) {
        for path in paths {
            let removed = memo
                .values
                .of(&path)
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&path);
            if removed.is_some() {
                entries.1 -= 1;
            }
        }
    }
}

/// Test hook: remember the published file `path` (below `<root>/actions/`)
/// as a cache check would, with its current device and inode.
#[cfg(test)]
pub(crate) fn remember_published_file(root: &Path, path: &Path) {
    let metadata = fs::symlink_metadata(path).unwrap();
    let input = Input {
        path: path.to_path_buf(),
        digest: Sha256Digest([0; 32]),
        size: metadata.len(),
        executable: 0o111,
        mode: OUTPUT_MODE,
    };
    let identity = Some((metadata.dev(), metadata.ino()));
    remember_outputs(
        root,
        vec![(path.to_path_buf(), PublishedValue::File(input, identity))],
    );
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
/// published from), as it was published at `root`: its files' paths below
/// `root`, with their published (blob) modes and no links
/// ([`publish_tree`]).
fn relocated(tree: Tree, root: &Path) -> Tree {
    Tree {
        files: tree
            .files
            .into_iter()
            .map(|(name, input)| {
                let path = root.join(&name);
                let mode = blob_mode(&input);
                (
                    name,
                    Input {
                        path,
                        mode,
                        ..input
                    },
                )
            })
            .collect(),
        directories: tree.directories,
        links: BTreeSet::new(),
    }
}

/// What [`read_staged`] returns for `path` when a per-process memo holds it
/// and no filesystem access is needed: a file of the immutable stores (no
/// published symlink to follow there) or a memoized published tree (a real
/// directory when it was read, and published trees stay in place).
fn memoized_staged(path: &Path, kind: InputKind, root: Option<&Path>) -> Option<Staged> {
    match kind {
        InputKind::Tree => published_output(root, path)
            .then(|| memoized_tree(path))
            .flatten()
            .map(Staged::Tree),
        InputKind::File if store_file(root, path) => memoized_file(path).map(Staged::File),
        InputKind::File => published_output(root, path)
            .then(|| memoized_output(path))
            .flatten()
            .and_then(|value| match value {
                PublishedValue::File(input, _) => Some(Staged::File(input)),
                PublishedValue::Link(_) => None,
            }),
        InputKind::SourceDirectory => memoized_source_directory(path).map(Staged::SourceDirectory),
        InputKind::Symlink => published_output(root, path)
            .then(|| memoized_output(path))
            .flatten()
            .and_then(|value| match value {
                PublishedValue::Link(text) => Some(Staged::Symlink(text)),
                PublishedValue::File(..) => None,
            }),
    }
}

/// An unresolved symlink artifact at `path` (a published symlink output):
/// its link text, read without following it
/// (FileArtifactValue.createForUnresolvedSymlink).
fn read_unresolved_symlink(path: &Path) -> Result<PathBuf, String> {
    let metadata = fs::symlink_metadata(path).map_err(|e| io_error("cannot inspect", path, e))?;
    if !metadata.file_type().is_symlink() {
        return Err(format!(
            "unresolved symlink input '{}' is not a symlink",
            path.display()
        ));
    }
    fs::read_link(path).map_err(|e| io_error("cannot read link", path, e))
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
/// tree memo (read and remembered on a miss); any other tree is read, its
/// symlinks followed ([`TreeRead::Input`]). A source directory is read once
/// per process ([`source_directory`]).
fn read_staged(path: &Path, kind: InputKind, root: Option<&Path>) -> Result<Staged, String> {
    match kind {
        InputKind::File => return Ok(Staged::File(read_input(path, root, true)?)),
        InputKind::SourceDirectory => {
            return Ok(Staged::SourceDirectory(source_directory(path, root)?));
        }
        InputKind::Symlink => return Ok(Staged::Symlink(read_unresolved_symlink(path)?)),
        InputKind::Tree => {}
    }
    let (resolved, _) = resolve_published(path, root)?;
    if !published_output(root, &resolved) {
        return Ok(Staged::Tree(Arc::new(read_tree(
            &resolved,
            root,
            TreeRead::Input,
        )?)));
    }
    if let Some(tree) = memoized_tree(&resolved) {
        return Ok(Staged::Tree(tree));
    }
    let tree = Arc::new(read_tree(&resolved, root, TreeRead::Published)?);
    remember_tree(resolved, Arc::clone(&tree));
    Ok(Staged::Tree(tree))
}

/// Artifact.prettyPrint (actions/Artifact.java:669-673) of the output at
/// `exec_path`: its root-relative path, below `bazel-out/<cfg>/<dir>`.
fn pretty_output(exec_path: &Path) -> String {
    let mut components = exec_path.components();
    let below_root = matches!(
        (components.next(), components.next(), components.next()),
        (
            Some(Component::Normal(out)),
            Some(Component::Normal(_)),
            Some(Component::Normal(_)),
        ) if out == "bazel-out"
    );
    if below_root && !components.as_path().as_os_str().is_empty() {
        components.as_path().display().to_string()
    } else {
        exec_path.display().to_string()
    }
}

/// Byte-ordered relative paths of a tree's regular files.
fn tree_listing(tree: &Tree) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = tree.files.keys().cloned().collect();
    files.sort_by(|a, b| a.as_os_str().as_bytes().cmp(b.as_os_str().as_bytes()));
    files
}

/// One source directory's memo slot: filled by the first reader while the
/// others wait for it (single flight); only a successful read is kept.
type SourceDirectorySlot = Arc<Mutex<Option<Arc<SourceDirectory>>>>;

/// Source directories read by this process, by path ([`source_directory`]).
/// Like Bazel's per-build artifact value of a source directory
/// (ArtifactFunction.createSourceValue, computed once per build), every
/// consumer in one command stages the value of the first read, so one build
/// never keys or stages two states of one directory. Bounded like the tree
/// memo (a memo that would exceed TREE_MEMO_LIMIT entries starts over).
#[derive(Default)]
struct SourceDirectoryMemo {
    slots: HashMap<PathBuf, SourceDirectorySlot>,
    /// Files, directories and dangling links of the filled slots.
    entries: usize,
}

fn source_directory_memo() -> &'static Mutex<SourceDirectoryMemo> {
    static MEMO: OnceLock<Mutex<SourceDirectoryMemo>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(SourceDirectoryMemo::default()))
}

fn source_directory_entries(directory: &SourceDirectory) -> usize {
    directory.files.len() + directory.directories.len() + directory.dangling.len()
}

/// The memoized value of the source directory at `path`, without waiting
/// for a read in progress.
fn memoized_source_directory(path: &Path) -> Option<Arc<SourceDirectory>> {
    let slot = source_directory_memo()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .slots
        .get(path)
        .cloned()?;
    let value = slot.try_lock().ok()?;
    value.clone()
}

/// The source directory at `path`: its memoized value, or read (once per
/// process, [`read_source_directory`]) and remembered.
fn source_directory(path: &Path, root: Option<&Path>) -> Result<Arc<SourceDirectory>, String> {
    let slot = Arc::clone(
        source_directory_memo()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .slots
            .entry(path.to_path_buf())
            .or_default(),
    );
    let mut value = slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(directory) = value.as_ref() {
        return Ok(Arc::clone(directory));
    }
    let directory = Arc::new(read_source_directory(path, root)?);
    *value = Some(Arc::clone(&directory));
    drop(value);
    let entries = source_directory_entries(&directory);
    let mut memo = source_directory_memo()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if memo.entries + entries > TREE_MEMO_LIMIT {
        memo.slots.retain(|_, other| Arc::ptr_eq(other, &slot));
        memo.entries = 0;
    }
    if entries <= TREE_MEMO_LIMIT {
        memo.entries += entries;
    } else {
        memo.slots.remove(path);
    }
    Ok(directory)
}

/// Walk a source directory like Bazel 9.3's source directory traversal
/// (ArtifactFunction.createSourceValue over RecursiveFilesystemTraversal
/// Function.traverseSourceChildren: one FileValue per entry, symlinks
/// followed): a symlink to a file is that file at the link's path
/// (ResolvedFileFactory.symlinkToFile, the target's metadata), a symlink to a
/// directory is walked at the link's path (symlinkToDirectory), a dangling
/// symlink is recorded with its link text (danglingSymlink), and every
/// directory is recorded (DirectoryArtifactTraversalRequest.emitEmptyDirectory
/// Nodes). A link chain that does not resolve (ELOOP) or a directory link back
/// into a directory it lies in fails (Bazel's FileSymlinkCycleException and
/// FileSymlinkInfiniteExpansionException, whose texts the planner reports
/// before execution, crate::engine); special files fail explicitly. Files
/// are read after the walk ([`try_map_ordered`]) at their own path, or a
/// linked one at its canonical path; the first failure in walk order is
/// reported.
fn read_source_directory(root: &Path, cache: Option<&Path>) -> Result<SourceDirectory, String> {
    let root_metadata = real_directory(root, cache)?;
    let owned = owned_by(cache, root);
    let mut directories = BTreeSet::new();
    let mut dangling = BTreeMap::new();
    // (relative path, where it is read, its lstat when read in place, owned)
    let mut files: Vec<(PathBuf, PathBuf, Option<fs::Metadata>, bool)> = Vec::new();
    // The directories the root lies in (itself included): a link back into
    // any of them expands without bound (FileFunction checks a link's
    // target against every ancestor of the link's path). `real_directory`
    // admitted no linked ancestor, so these are the root's real ancestors.
    let mut root_ancestors = vec![(root_metadata.dev(), root_metadata.ino())];
    for ancestor in root.ancestors().skip(1) {
        if let Ok(metadata) = fs::metadata(ancestor) {
            root_ancestors.push((metadata.dev(), metadata.ino()));
        }
    }
    let walk = || -> Result<(), String> {
        // Directories to list: the relative path, whether a link led there,
        // and the identities of the directories it lies in (itself included).
        let mut pending = vec![(PathBuf::new(), false, root_ancestors)];
        while let Some((relative, linked, ancestors)) = pending.pop() {
            let directory = root.join(&relative);
            let mut names = fs::read_dir(&directory)
                .map_err(|e| io_error("cannot list directory", &directory, e))?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| io_error("cannot list directory", &directory, e))?;
            names.sort();
            for name in names {
                let child = relative.join(&name);
                let path = root.join(&child);
                let metadata = fs::symlink_metadata(&path)
                    .map_err(|e| io_error("cannot inspect", &path, e))?;
                let through_link = linked || metadata.file_type().is_symlink();
                let metadata = if metadata.file_type().is_symlink() {
                    match fs::metadata(&path) {
                        Ok(resolved) => resolved,
                        Err(e)
                            if e.kind() == io::ErrorKind::NotFound
                                || e.raw_os_error()
                                    == Some(rustix::io::Errno::NOTDIR.raw_os_error()) =>
                        {
                            let target = fs::read_link(&path)
                                .map_err(|e| io_error("cannot read link", &path, e))?;
                            dangling.insert(child, target);
                            continue;
                        }
                        Err(e)
                            if e.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error()) =>
                        {
                            return Err(format!(
                                "source directory entry '{}': Symlink cycle",
                                path.display()
                            ));
                        }
                        Err(e) => return Err(io_error("cannot inspect", &path, e)),
                    }
                } else {
                    metadata
                };
                if metadata.is_dir() {
                    let identity = (metadata.dev(), metadata.ino());
                    if ancestors.contains(&identity) {
                        return Err(format!(
                            "source directory entry '{}': Infinite symlink expansion",
                            path.display()
                        ));
                    }
                    let mut inner = ancestors.clone();
                    inner.push(identity);
                    directories.insert(child.clone());
                    pending.push((child, through_link, inner));
                } else if metadata.is_file() && through_link {
                    // Read where the links lead (ResolvedFile metadata of
                    // the resolved file).
                    let target = fs::canonicalize(&path)
                        .map_err(|e| io_error("cannot resolve", &path, e))?;
                    let owned = owned_by(cache, &target);
                    files.push((child, target, None, owned));
                } else if metadata.is_file() {
                    files.push((child, path, Some(metadata), owned));
                } else {
                    return Err(format!(
                        "source directory entry '{}' is not a regular file, directory or symlink; such entries are not supported by the native profile",
                        path.display()
                    ));
                }
            }
        }
        Ok(())
    };
    let walked = walk();
    let inputs = try_map_ordered(&files, |(_, path, metadata, owned)| match metadata {
        Some(metadata) => read_regular_from(path, metadata.clone(), *owned, false),
        None => fs::symlink_metadata(path)
            .map_err(|e| io_error("cannot inspect", path, e))
            .and_then(|metadata| read_regular_from(path, metadata, *owned, false)),
    });
    let mut directory = SourceDirectory {
        files: BTreeMap::new(),
        directories,
        dangling,
    };
    for ((child, _, _, _), input) in files.into_iter().zip(inputs) {
        directory.files.insert(child, input?);
    }
    walked?;
    Ok(directory)
}

struct PreparedAction {
    inputs: BTreeMap<PathBuf, Staged>,
    /// Absolute input paths by destination (the first mapping of a
    /// duplicated destination): symlink outputs point at them. Only
    /// transforms ([`run_transform`] and their keys) read them: empty for
    /// a process action whose sources are all absolute ([`prepare`]).
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
/// parameter files) are equal or one contains the other: one hashed lookup
/// per path and proper ancestor (component-wise, like `Path::starts_with`),
/// linear in the large input sets of toolchains, SDKs and node_modules trees.
///
/// Precondition: every path passed [`validate_relative`].
fn namespace_may_overlap(action: &Action) -> bool {
    // Every path passed validate_relative: relative, normal components,
    // single separators, never empty. Such paths are equal exactly when
    // their bytes are, and their proper ancestors are the prefixes that end
    // before a separator, so the lookups need neither component parsing nor
    // `Path` hashing.
    let count = action.inputs.len() + action.embedded_inputs.len();
    let mut paths: crate::fast_hash::FxHashSet<&[u8]> =
        crate::fast_hash::set_with_capacity(count + action.outputs.len());
    for destination in action
        .inputs
        .iter()
        .map(|(_, destination)| destination.as_os_str().as_bytes())
        .chain(
            action
                .embedded_inputs
                .iter()
                .map(|(destination, _)| destination.as_os_str().as_bytes()),
        )
    {
        paths.insert(destination);
    }
    let others = action
        .outputs
        .iter()
        .map(|path| path.as_os_str().as_bytes())
        .chain(
            action
                .extra
                .param_files
                .iter()
                .map(|(path, _)| path.as_os_str().as_bytes()),
        );
    for path in others {
        if !paths.insert(path) {
            return true;
        }
    }
    paths.iter().any(|path| {
        path.iter()
            .enumerate()
            .any(|(at, byte)| *byte == b'/' && paths.contains(&path[..at]))
    })
}

pub fn validate_structure(action: &Action) -> Result<(), String> {
    if let Some(java) = &action.extra.java_compile {
        java.validate(action)?;
    }
    if action.write.is_some()
        && (!action.command.is_empty()
            || action.program.is_some()
            || !action.arguments.is_empty()
            || !action.environment.is_empty()
            // Only tree inputs, which written Args expand (ParameterFileWriteAction).
            || action
                .inputs
                .iter()
                .any(|(_, name)| !action.extra.tree_inputs.contains(name.as_path()))
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
        Some(Transform::UnresolvedSymlink { .. }) => {
            check_one_output(action, "unresolved symlink")?;
            if !action.inputs.is_empty() || !action.extra.tree_outputs.is_empty() {
                return Err("unresolved symlink action has no inputs and a symlink output".into());
            }
            if !action.extra.symlink_outputs.contains(&action.outputs[0]) {
                return Err("unresolved symlink action output must be a symlink artifact".into());
            }
        }
        // The registration-time shape; planning rewrites it into a Template
        // (crate::engine::resolve_build_info) before plan_action validates
        // the template input.
        Some(Transform::BuildInfo { .. }) => check_one_output(action, "build info")?,
        // A planned request's placeholder; finalize replaces it.
        Some(Transform::WorkspaceStatus { .. }) => check_one_output(action, "workspace status")?,
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
    // One hashed lookup per declared tree/directory/symlink input (an
    // action with thousands of node_modules links and package directories
    // is checked in linear time, not against every input).
    let input_names: std::cell::OnceCell<crate::fast_hash::FxHashSet<&Path>> =
        std::cell::OnceCell::new();
    let is_input = |path: &Path| {
        input_names
            .get_or_init(|| {
                action
                    .inputs
                    .iter()
                    .map(|(_, name)| name.as_path())
                    .collect()
            })
            .contains(path)
    };
    for tree in &action.extra.tree_inputs {
        if !is_input(tree) {
            return Err(format!(
                "tree input '{}' is not an action input",
                tree.display()
            ));
        }
    }
    for directory in &action.extra.source_directories {
        if !is_input(directory) || action.extra.tree_inputs.contains(directory) {
            return Err(format!(
                "source directory '{}' is not a non-tree action input",
                directory.display()
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
    for link in &action.extra.symlink_outputs {
        if !action.outputs.contains(link) || action.extra.tree_outputs.contains(link) {
            return Err(format!(
                "symlink output '{}' is not a non-tree action output",
                link.display()
            ));
        }
    }
    if action.write.is_some() && !action.extra.symlink_outputs.is_empty() {
        return Err("write action output cannot be a symlink artifact".into());
    }
    for link in &action.extra.symlink_inputs {
        if !is_input(link)
            || action.extra.tree_inputs.contains(link)
            || action.extra.source_directories.contains(link)
        {
            return Err(format!(
                "symlink input '{}' is not a file action input",
                link.display()
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
        .map(|(_, destination)| destination.as_path())
        .chain(
            action
                .embedded_inputs
                .iter()
                .map(|(destination, _)| destination.as_path()),
        )
        .chain(action.outputs.iter().map(PathBuf::as_path))
        .chain(
            action
                .extra
                .param_files
                .iter()
                .map(|(path, _)| path.as_path()),
        )
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
        .map(|(_, destination)| destination.as_path())
        .chain(
            action
                .embedded_inputs
                .iter()
                .map(|(destination, _)| destination.as_path()),
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
        input_names.push(destination.to_path_buf());
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
fn check_runfiles_entries(action: &Action, entries: &RunfilesEntries) -> Result<(), String> {
    // One lookup per entry (a runfiles tree of 30,000 entries is checked in
    // linear time, not against every input).
    let inputs: HashSet<&Path> = action
        .inputs
        .iter()
        .map(|(_, name)| name.as_path())
        .collect();
    for (path, destination) in entries.iter() {
        validate_relative(Path::new(&**path))
            .map_err(|e| format!("invalid runfiles path '{path}': {e}"))?;
        if let Some(destination) = destination
            && !inputs.contains(destination.as_path())
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
    entries: &RunfilesEntries,
    origins: &BTreeMap<PathBuf, PathBuf>,
) {
    frame_count(hash, entries.len());
    for (path, destination) in entries.iter() {
        frame(hash, path.as_bytes());
        match destination {
            Some(destination) => {
                frame(hash, b"link");
                frame(hash, origins[destination.as_path()].as_os_str().as_bytes());
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
        // Everything Bazel's source directory fingerprint covers
        // (ArtifactFunction.createSourceValue: each traversed entry's path
        // and digest, empty directories and dangling links by link text),
        // plus the executable bits native staging carries.
        Staged::SourceDirectory(directory) => {
            frame(hash, b"source-directory");
            frame_count(hash, directory.files.len());
            for (name, input) in &directory.files {
                frame(hash, name.as_os_str().as_bytes());
                frame_input(hash, input);
            }
            frame_count(hash, directory.directories.len());
            for name in &directory.directories {
                frame(hash, name.as_os_str().as_bytes());
            }
            frame_count(hash, directory.dangling.len());
            for (name, target) in &directory.dangling {
                frame(hash, name.as_os_str().as_bytes());
                frame(hash, target.as_os_str().as_bytes());
            }
        }
        // UnresolvedSymlinkArtifactValue's digest is a function of the link
        // text alone.
        Staged::Symlink(text) => {
            frame(hash, b"unresolved-symlink");
            frame(hash, text.as_os_str().as_bytes());
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
    // their manifest and executable (the other inputs are keyed by path),
    // and the link text of unresolved symlinks, which they reproduce
    // (SourceManifestAction.writeFile, SymlinkTreeHelper
    // .createRunfilesSymlinks).
    let read = |destination: &Path| match paths_only {
        None => true,
        Some(_) if action.extra.symlink_inputs.contains(destination) => true,
        Some(Transform::RunfilesTree {
            manifest,
            executable,
            ..
        }) => manifest == destination || executable.as_deref() == Some(destination),
        Some(_) => false,
    };
    let reads: Vec<(&Path, InputKind)> = action
        .inputs
        .iter()
        .filter(|(_, destination)| read(destination))
        .map(|(source, destination)| {
            let kind = if action.extra.tree_inputs.contains(destination.as_path()) {
                InputKind::Tree
            } else if action
                .extra
                .source_directories
                .contains(destination.as_path())
            {
                InputKind::SourceDirectory
            } else if action.extra.symlink_inputs.contains(destination.as_path()) {
                InputKind::Symlink
            } else {
                InputKind::File
            };
            (source.as_path(), kind)
        })
        .collect();
    // Inputs the per-process memos hold need no filesystem access; the
    // others are read up front (many of them, such as an SDK being hashed,
    // on several threads). The loop consumes the results in input order, so
    // it fails exactly where a sequential read would.
    let mut staged: Vec<Option<Result<Staged, String>>> = reads
        .iter()
        .map(|&(source, kind)| memoized_staged(source, kind, root).map(Ok))
        .collect();
    let misses: Vec<usize> = (0..staged.len())
        .filter(|&index| staged[index].is_none())
        .collect();
    // Each action reads its misses from its own starting point: the
    // actions that start together miss the same files (a toolchain, an npm
    // package's sources), and in one order they would queue behind each
    // other's single-flight reads ([`read_input`]); every miss is read
    // (no early stop), so the first failure in input order is still the one
    // reported below.
    let mut misses = misses;
    if misses.len() > PARALLEL_ITEMS_PER_THREAD {
        let offset = usize::try_from(crate::digest_memo::path_hash(Path::new(&action.id)))
            .unwrap_or(0)
            % misses.len();
        misses.rotate_left(offset);
    }
    let read_misses = try_map_all(&misses, |&index| {
        let (source, kind) = reads[index];
        read_staged(source, kind, root)
    });
    for (&index, result) in misses.iter().zip(read_misses) {
        staged[index] = Some(result);
    }
    // The first mapping of each destination (by bytes: validate_structure
    // admitted only relative normal components, so equal bytes are equal
    // paths), in input order, so that failures surface exactly where they
    // did; the ordered maps are then built from sorted runs instead of one
    // component-wise search per input (6,000-input actions).
    //
    // Only transforms read the origins (their keys and run_transform); a
    // process action keeps none. Resolving an absolute source cannot fail
    // (std::path::absolute needs the working directory only for a
    // relative one), so skipping it for such an action changes no result;
    // any relative source takes the resolving loop as before.
    let keep_origins = action.extra.transform.is_some()
        || action
            .inputs
            .iter()
            .any(|(source, _)| source.as_os_str().as_bytes().first() != Some(&b'/'));
    let mut staged = staged.into_iter();
    let mut first_origins: FxHashSet<&[u8]> =
        crate::fast_hash::set_with_capacity(if keep_origins { action.inputs.len() } else { 0 });
    let mut origin_entries: Vec<(&Path, PathBuf)> =
        Vec::with_capacity(if keep_origins { action.inputs.len() } else { 0 });
    let mut input_slots: FxHashMap<&[u8], usize> =
        HashMap::with_capacity_and_hasher(reads.len(), Default::default());
    let mut input_entries: Vec<(&Path, Staged)> = Vec::with_capacity(reads.len());
    for (source, destination) in &action.inputs {
        let name = destination.as_os_str().as_bytes();
        if keep_origins && first_origins.insert(name) {
            origin_entries.push((
                destination.as_path(),
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
                input_entries.push((destination.as_path(), input));
            }
        }
    }
    let by_destination =
        |a: &Path, b: &Path| compare_relative(a.as_os_str().as_bytes(), b.as_os_str().as_bytes());
    origin_entries.sort_unstable_by(|a, b| by_destination(a.0, b.0));
    input_entries.sort_unstable_by(|a, b| by_destination(a.0, b.0));
    // Sorted and unique: collecting checks the order in one pass.
    let origins: BTreeMap<PathBuf, PathBuf> = origin_entries
        .into_iter()
        .map(|(destination, origin)| (destination.to_path_buf(), origin))
        .collect();
    let inputs: BTreeMap<PathBuf, Staged> = input_entries
        .into_iter()
        .map(|(destination, input)| (destination.to_path_buf(), input))
        .collect();
    let mut hash = Sha256::new();
    for value in [FORMAT, std::env::consts::OS, std::env::consts::ARCH] {
        frame(&mut hash, value.as_bytes());
    }
    // A constant: the executed TMPDIR (local_tmpdir) is not keyed, like
    // Bazel's rewriteLocalEnv, which runs after the action key. The
    // historical spelling is kept so existing cache entries stay valid.
    frame(&mut hash, b"TMPDIR=fresh-absolute-work-tmp");
    // Not the action's id (its owner and index, SURVEY3 S1): Bazel's action
    // key has no owner (ActionKeyComputer.getKey over computeKey, e.g.
    // SpawnAction.computeKey :396-417 — the command lines, the mnemonic, the
    // environment and the execution info), so a shared action whose owner
    // depends on the requested targets (crate::shared_actions; rules_js's
    // node_modules links) keeps its entry. Its outputs (keyed below) name
    // it: two owners registering one output path share one action or are
    // rejected at analysis (crate::engine register_output).
    frame(&mut hash, action.extra.mnemonic.as_bytes());
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
        (
            None,
            _,
            Some(Transform::UnresolvedSymlink {
                target,
                target_type,
                ..
            }),
        ) => {
            // UnresolvedSymlinkAction.computeKey (:92-99).
            frame(&mut hash, b"unresolved-symlink");
            frame(&mut hash, target.as_bytes());
            frame(&mut hash, target_type.name().as_bytes());
        }
        (None, _, Some(Transform::BuildInfo { volatile })) => {
            // Replaced during planning (crate::engine); keyed like the
            // resolved action would be, so a stray registration cannot
            // collide with either transform.
            frame(&mut hash, b"build-info");
            frame(&mut hash, &[u8::from(*volatile)]);
        }
        (None, _, Some(Transform::WorkspaceStatus { volatile })) => {
            // Replaced by the planned request's finalize (crate::request);
            // keyed apart so a stray placeholder cannot collide.
            frame(&mut hash, b"workspace-status-placeholder");
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
    // Only actions with source directory inputs frame this block, so every
    // other key is unchanged.
    if !extra.source_directories.is_empty() {
        frame(&mut hash, b"source-directories");
        frame_count(&mut hash, extra.source_directories.len());
        for name in &extra.source_directories {
            frame(&mut hash, name.as_os_str().as_bytes());
        }
    }
    // Likewise only actions reading or creating unresolved symlinks.
    if !extra.symlink_inputs.is_empty() || !extra.symlink_outputs.is_empty() {
        frame(&mut hash, b"unresolved-symlinks");
        frame_count(&mut hash, extra.symlink_inputs.len());
        for name in &extra.symlink_inputs {
            frame(&mut hash, name.as_os_str().as_bytes());
        }
        frame_count(&mut hash, extra.symlink_outputs.len());
        for name in &extra.symlink_outputs {
            frame(&mut hash, name.as_os_str().as_bytes());
        }
    }
    if let Some(java) = &extra.java_compile {
        frame(&mut hash, b"java-compile-v1");
        frame(
            &mut hash,
            &postcard::to_stdvec(java).map_err(|e| e.to_string())?,
        );
    }
    let key = hex(&hash.finalize());
    Ok(PreparedAction {
        inputs,
        origins,
        outputs,
        key,
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
/// Removes a directory tree whose directories may be read-only.
pub(crate) fn remove_tree(path: &Path) -> io::Result<()> {
    remove_directory(path)
}

/// Removes everything inside `path`, keeping the directory itself (a
/// persistent worker's working directory, its process's cwd).
fn clear_directory(path: &Path) -> Result<(), String> {
    let entries = fs::read_dir(path).map_err(|e| io_error("cannot read directory", path, e))?;
    for entry in entries {
        let entry = entry.map_err(|e| io_error("cannot read directory", path, e))?;
        let child = entry.path();
        let removed = if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            remove_directory(&child)
        } else {
            fs::remove_file(&child)
        };
        removed.map_err(|e| io_error("cannot remove", &child, e))?;
    }
    Ok(())
}

/// A persistent worker's WorkRequest inputs (WorkerSpawnRunner
/// .createWorkRequest over InputMetadataProvider.expandArtifacts): every
/// input file with its SHA-256 as lowercase hex, tree artifacts expanded to
/// their files, runfiles trees left out, a source directory listed without
/// a digest; then the parameter files. Sorted by execution path (genuine
/// lists the spawn's inputs in nested-set order: inputs, tools, parameter
/// files).
fn worker_request_inputs(
    inputs: &BTreeMap<PathBuf, Staged>,
    param_files: &[(PathBuf, Vec<u8>)],
) -> Vec<(String, String)> {
    let runfiles = |name: &Path| {
        name.components().any(|component| {
            component
                .as_os_str()
                .to_string_lossy()
                .ends_with(".runfiles")
        })
    };
    let path = |name: &Path| name.to_string_lossy().into_owned();
    let mut listed = Vec::new();
    for (name, input) in inputs {
        if runfiles(name) {
            continue;
        }
        match input {
            Staged::File(input) => listed.push((path(name), input.digest.hex())),
            Staged::Tree(tree) => {
                for (file, input) in &tree.files {
                    listed.push((path(&name.join(file)), input.digest.hex()));
                }
            }
            Staged::SourceDirectory(_) => listed.push((path(name), String::new())),
            Staged::Symlink(text) => listed.push((
                path(name),
                unresolved_symlink_digest(text.as_os_str().as_bytes()).hex(),
            )),
        }
    }
    for (name, content) in param_files {
        listed.push((path(name), digest(content).hex()));
    }
    listed
}

/// Whether `path` (an execution path) is a declared input of the spawn: a
/// staged input, a file of a tree input, or a parameter file
/// (WorkerSpawnRunner.expandArgument reads only those).
fn declared_input(
    inputs: &BTreeMap<PathBuf, Staged>,
    param_files: &[(PathBuf, Vec<u8>)],
    path: &Path,
) -> bool {
    if inputs.contains_key(path) || param_files.iter().any(|(name, _)| name == path) {
        return true;
    }
    path.ancestors()
        .skip(1)
        .any(|ancestor| match inputs.get(ancestor) {
            Some(Staged::Tree(tree)) => path
                .strip_prefix(ancestor)
                .is_ok_and(|file| tree.files.contains_key(file)),
            _ => false,
        })
}

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
/// file with the right size and digest. A damaged blob is removed. An
/// intact blob's digest is known by its path from then on
/// ([`remember_owned`]): its name is its content, and this process forgets
/// it whenever it removes or replaces the blob.
fn blob_valid(blob: &Path, input: &Input) -> Result<bool, String> {
    let metadata = match fs::symlink_metadata(blob) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io_error("cannot inspect", blob, e)),
    };
    let intact = metadata.is_file()
        && metadata.permissions().mode() & 0o7777 == blob_mode(input)
        && metadata.size() == input.size
        && match memoized_digest(blob, &file_key(&metadata, true)) {
            Some(digest) => digest == input.digest,
            None => {
                let intact = hash_file_from(blob, metadata.clone(), true, false)
                    .is_ok_and(|(digest, _)| digest == input.digest);
                if intact {
                    remember_owned(blob, &metadata, input.digest, None);
                }
                intact
            }
        };
    if !intact {
        forget_produced(blob);
        remove_stale(blob)?;
    }
    Ok(intact)
}

/// Whether `path` lies in the repository store below the canonical cache
/// root (`repositories/`). Its trees keep exactly one link per file: the
/// direct store rejects a hard-linked file when it verifies a tree
/// (crate::repository_store `regular_metadata`), so a blob linked to one
/// would make that repository re-execute and be republished on the next
/// command. Their bytes are copied into the store like any external file.
fn in_repository_store(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root)
        .is_ok_and(|rest| rest.starts_with("repositories"))
}

/// The store blob of `input`, created on first use: by hardlinking a file
/// the cache owns that already has the blob's mode (a published output or
/// fresh execution-root output; never a repository's file,
/// [`in_repository_store`]), otherwise by one verified copy. Concurrent
/// creators agree on the first linked inode.
fn ensure_blob(cas: &Path, input: &Input, root: &Path) -> Result<PathBuf, String> {
    ensure_blob_hashed(cas, input, root, None)
}

/// [`ensure_blob`] for an input whose digest this process computed by
/// reading its file in the state `hashed` (an output being published): a
/// blob linked from that very state is not read again. A blob this process
/// creates is known by its path from then on ([`remember_owned`]), in the
/// state its content was established in (read, or copied while hashed).
fn ensure_blob_hashed(
    cas: &Path,
    input: &Input,
    root: &Path,
    hashed: Option<FileKey>,
) -> Result<PathBuf, String> {
    let blob = blob_path(cas, input);
    if blob_valid(&blob, input)? {
        return Ok(blob);
    }
    // Workers of one process that need the same new blob create it once
    // (the others wait, then find it valid) instead of copying in parallel.
    static STRIPES: [Mutex<()>; 64] = [const { Mutex::new(()) }; 64];
    let _creating = creating(&STRIPES, input);
    if blob_valid(&blob, input)? {
        return Ok(blob);
    }
    let candidate = temporary(cas, &input.digest.hex());
    let linked = input.path.starts_with(root)
        && !in_repository_store(root, &input.path)
        && input.mode == blob_mode(input)
        && hard_link(&input.path, &candidate).is_ok();
    let state = if linked {
        let metadata = fs::symlink_metadata(&candidate)
            .map_err(|e| io_error("cannot inspect", &candidate, e))?;
        // The input's own inode: read again unless this process read exactly
        // this state of it (the caller, or the memo of the input's path).
        let key = file_key(&metadata, true);
        if hashed != Some(key) && memoized_digest(&input.path, &key) != Some(input.digest) {
            let (digest, _) = hash_file_from(&candidate, metadata.clone(), true, false)?;
            if digest != input.digest {
                let _ = fs::remove_file(&candidate);
                return Err(format!(
                    "input '{}' changed while it was being staged",
                    input.path.display()
                ));
            }
        }
        metadata
    } else {
        copy_candidate(&input.path, &candidate, input)?
    };
    let stored = store_candidate(&candidate, &blob, || blob_valid(&blob, input));
    if stored? {
        remember_created(&blob, &state, input.digest);
    }
    Ok(blob)
}

/// The lock of `stripes` for `input`'s content, held while creating one of
/// its store files.
fn creating<'a>(stripes: &'a [Mutex<()>; 64], input: &Input) -> std::sync::MutexGuard<'a, ()> {
    let stripe = usize::from(input.digest.0[0]) % stripes.len();
    stripes[stripe].lock().unwrap_or_else(|e| e.into_inner())
}

/// Copy `source` to the new store candidate `candidate` for `input`
/// (verified: the copy must hold the keyed digest), with the blob's mode.
/// Returns the candidate's lstat.
fn copy_candidate(source: &Path, candidate: &Path, input: &Input) -> Result<fs::Metadata, String> {
    copy_verified(source, candidate, input.digest)?;
    if input.executable != 0 {
        // The verified copy hashes while it writes, too slow for the
        // exclusive spawn lock; re-create the executable blob as an inode
        // no child can hold open for writing (a kernel copy).
        crate::spawn_lock::seal(candidate).map_err(|e| io_error("cannot seal", candidate, e))?;
    }
    set_mode(candidate, blob_mode(input))?;
    fs::symlink_metadata(candidate).map_err(|e| io_error("cannot inspect", candidate, e))
}

/// The `ordinal`th distinct copy (from 1) of `input`'s blob,
/// `cas/<digest>-<mode>.<ordinal>`: the blob's bytes and mode in an inode
/// of its own, for the `ordinal`th further distinct artifact with that
/// content in one execution root ([`DistinctInodes`]).
fn copy_path(cas: &Path, input: &Input, ordinal: usize) -> PathBuf {
    cas.join(format!("{}-{:o}.{ordinal}", input.digest, blob_mode(input)))
}

/// [`copy_path`] of `input`, created on first use by a verified copy of its
/// blob ([`ensure_blob`]) and from then on kept like a blob: valid while it
/// is a read-only regular file with the blob's mode, size and digest
/// ([`blob_valid`]), known by its path to this process once created or
/// validated, removed when damaged ([`check_staged`]).
fn ensure_copy(cas: &Path, input: &Input, root: &Path, ordinal: usize) -> Result<PathBuf, String> {
    let copy = copy_path(cas, input, ordinal);
    if blob_valid(&copy, input)? {
        return Ok(copy);
    }
    let blob = ensure_blob(cas, input, root)?;
    // Not ensure_blob's stripes: it holds them while creating the blob.
    static STRIPES: [Mutex<()>; 64] = [const { Mutex::new(()) }; 64];
    let _creating = creating(&STRIPES, input);
    if blob_valid(&copy, input)? {
        return Ok(copy);
    }
    let candidate = temporary(cas, &input.digest.hex());
    let state = copy_candidate(&blob, &candidate, input)?;
    if store_candidate(&candidate, &copy, || blob_valid(&copy, input))? {
        remember_created(&copy, &state, input.digest);
    }
    Ok(copy)
}

/// Move the store candidate `candidate` into place as `blob` (a hardlink;
/// over an invalid blob, or where hardlinks fail, a rename) and remove the
/// candidate's name. Whether `blob` is now the candidate's inode: not when a
/// concurrent creator's blob is there and `valid`.
fn store_candidate(
    candidate: &Path,
    blob: &Path,
    valid: impl FnOnce() -> Result<bool, String>,
) -> Result<bool, String> {
    let rename = || {
        // Replaced: whatever this process knew of the file there is void.
        forget_produced(blob);
        fs::rename(candidate, blob)
            .map(|()| true)
            .map_err(|e| io_error("cannot store", blob, e))
    };
    let stored = match fs::hard_link(candidate, blob) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => match valid() {
            Ok(true) => Ok(false),
            Ok(false) => rename(),
            Err(error) => Err(error),
        },
        // A filesystem without hardlinks still allows a rename.
        Err(_) => rename(),
    };
    let _ = fs::remove_file(candidate);
    stored
}

/// `blob`, which this process just created as the inode it established
/// the content `digest` of in the state `state`, is known by its path from
/// now on ([`remember_owned`]) when it still shows that state.
fn remember_created(blob: &Path, state: &fs::Metadata, digest: Sha256Digest) {
    if let Ok(metadata) = fs::symlink_metadata(blob)
        && metadata.is_file()
        && file_key(&metadata, true) == file_key(state, true)
    {
        remember_owned(blob, &metadata, digest, None);
    }
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
    link_ensured(|| ensure_blob(cas, input, root), target, mode)
}

/// [`link_ensured_blob`] of the store file `ensure` returns (a blob, or a
/// distinct copy of one, [`ensure_copy`]).
fn link_ensured(
    ensure: impl Fn() -> Result<PathBuf, String>,
    target: &Path,
    mode: u32,
) -> Result<PathBuf, String> {
    let blob = ensure()?;
    #[cfg(test)]
    if REMOVE_BLOB_BEFORE_LINK.with(|hook| hook.replace(false)) {
        let _ = fs::remove_file(&blob);
    }
    match link_or_copy_detail(&blob, target, mode) {
        Ok(()) => Ok(blob),
        Err(LinkFailure::BlobMissing(_)) => {
            let blob = ensure()?;
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

/// A staged input file: its destination, the file it shares its inode with
/// (a store blob, or the repository tree file a link names) and its state.
struct StagedFile {
    destination: PathBuf,
    blob: PathBuf,
    state: InodeState,
    /// A link to a repository tree's file ([`stage_tree_link`]): the keyed
    /// digest, which that path must still hold after the process (with the
    /// mode in `state`); the file is never removed. `None` for a `cas/`
    /// blob, which a modification removes.
    tree_link: Option<Sha256Digest>,
}

/// The distinct artifacts staged from each store blob into one execution
/// root (an action's, or a test's runfiles), in staging order. Identical
/// contents share one blob, but distinct artifacts are distinct files in
/// Bazel: its sandboxes link every input to its own execroot file or
/// source, its runfiles trees link to those, and its outputs are separate
/// files. One inode for two of them made `tar`, `bsdtar`/mtree, `cp -a`
/// and `rsync -H` record a hard link between them (rules_js's
/// `js_image_layer` stored a copied `package.json` as a hard link to the
/// identical file of a Next.js standalone tree, and a consumer extracting
/// only the former lost it). The first artifact of a content is staged
/// from the blob, the `n`th further one from the blob's `n`th distinct
/// copy ([`ensure_copy`]); one artifact keeps one inode at all its paths
/// (its execution path, runfiles, `actions.symlink` aliases, which resolve
/// to the artifact they name). Keyed by the blob's content and mode, and by
/// the artifact's resolved path, borrowed from the staged inputs.
#[derive(Default)]
struct DistinctInodes<'a>(HashMap<(Sha256Digest, u32), Artifacts<'a>>);

/// The distinct artifacts of one blob: the first, then the further ones by
/// their ordinal (from 1).
struct Artifacts<'a> {
    first: &'a Path,
    further: HashMap<&'a Path, usize>,
}

impl<'a> DistinctInodes<'a> {
    /// The ordinal of `input`'s artifact (its resolved path) among the
    /// distinct artifacts staged from its blob so far: 0 for the first.
    fn ordinal(&mut self, input: &'a Input) -> usize {
        let artifact = input.path.as_path();
        match self.0.entry((input.digest, blob_mode(input))) {
            hash_map::Entry::Vacant(entry) => {
                entry.insert(Artifacts {
                    first: artifact,
                    further: HashMap::new(),
                });
                0
            }
            hash_map::Entry::Occupied(entry) => {
                let artifacts = entry.into_mut();
                if artifacts.first == artifact {
                    return 0;
                }
                let next = artifacts.further.len() + 1;
                *artifacts.further.entry(artifact).or_insert(next)
            }
        }
    }
}

fn stage_file<'a>(
    cas: &Path,
    root: &Path,
    input: &'a Input,
    execroot: &Path,
    destination: &Path,
    staged: &mut Vec<StagedFile>,
    distinct: &mut DistinctInodes<'a>,
) -> Result<(), String> {
    let target = execroot.join(destination);
    if let Some(parent) = target.parent() {
        ensure_directory(parent)?;
    }
    let blob = match distinct.ordinal(input) {
        0 => link_ensured_blob(cas, root, input, &target, blob_mode(input))?,
        ordinal => link_ensured(
            || ensure_copy(cas, input, root, ordinal),
            &target,
            blob_mode(input),
        )?,
    };
    let metadata =
        fs::symlink_metadata(&target).map_err(|e| io_error("cannot inspect", &target, e))?;
    staged.push(StagedFile {
        destination: destination.to_path_buf(),
        blob,
        state: inode_state(&metadata),
        tree_link: None,
    });
    Ok(())
}

/// Stage `input`, a file of a published repository tree, as an absolute
/// symlink to that file instead of a hardlinked blob, wherever the process
/// sees it: at its own path (`external/<repo>/...`), as an
/// `actions.symlink` alias's generated artifact, or as a runfile (a tool's
/// runfiles tree in an action, a test's runfiles). Bazel presents such a
/// file as a symlink into `<output_base>/external/<repo>` (the runfiles
/// tree's links, SymlinkTreeHelper; the symlinked sandbox,
/// SymlinkedSandboxedSpawn; the execroot's `external/` symlink forest), so a
/// program that finds its installation through its own resolved path finds
/// it complete in the tree: rules_nodejs's `bin/node` wrapper follows its
/// own links and runs the undeclared `nodejs/bin/node` beside it, CPython
/// searches `lib/python3.X` from `realpath(argv[0])` (Modules/getpath.py),
/// and rules_python's PyCompile runs `bazel-out/.../python/private/python3`,
/// an alias of the interpreter (`current_interpreter_executable`) whose
/// blob, staged there, resolved to itself: `Could not find platform
/// independent libraries`.
/// Once linked, the tree's file must still hold exactly the keyed input —
/// its size, mode and content, which is hashed again (see below) like a
/// blob's — and after the process the file at its path must be that inode
/// unchanged or a file with the keyed content and mode ([`check_staged`]).
fn stage_tree_link(
    input: &Input,
    execroot: &Path,
    destination: &Path,
    staged: &mut Vec<StagedFile>,
) -> Result<(), String> {
    let target = execroot.join(destination);
    if let Some(parent) = target.parent() {
        ensure_directory(parent)?;
    }
    std::os::unix::fs::symlink(&input.path, &target)
        .map_err(|e| io_error("cannot link", &target, e))?;
    let metadata = fs::symlink_metadata(&input.path)
        .map_err(|e| io_error("cannot inspect", &input.path, e))?;
    let changed = || {
        format!(
            "input '{}' changed while it was being staged",
            input.path.display()
        )
    };
    if !metadata.is_file()
        || metadata.size() != input.size
        || metadata.permissions().mode() & 0o7777 != input.mode
    {
        return Err(changed());
    }
    // The key took the digest from the file memo, which keeps a store
    // file's digest by its path for the whole process ([`read_input`]): a
    // tree file rewritten in place since its first read would run its new
    // bytes under a key computed from the old ones (blob staging stages
    // the blob of the keyed bytes instead). Hashed by its change-time key,
    // not as a cache-owned file: the per-process digest memo then answers
    // only for the unchanged file, never for an in-place rewrite that kept
    // the size and restored the modification time (the persistent memo
    // keys repository tree files so as well, crate::digest_memo).
    let (digest, metadata) = hash_file_from(&input.path, metadata, false, false)?;
    if digest != input.digest {
        return Err(changed());
    }
    staged.push(StagedFile {
        destination: destination.to_path_buf(),
        blob: input.path.clone(),
        state: inode_state(&metadata),
        tree_link: Some(input.digest),
    });
    Ok(())
}

/// Staging totals of this process, reported under `BAZEL_RS_TIMING`
/// ([`report_staging`]): calls of [`stage_inputs`], their summed wall time
/// (over all workers) in microseconds, and the files staged as links into
/// repository trees and as store blobs.
static STAGING: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// The `PERF-STAGE staging` line when `BAZEL_RS_TIMING` is set: what input
/// staging (actions and tests) cost this process.
pub fn report_staging() {
    if !crate::diagnostic_switch(std::env::var_os("BAZEL_RS_TIMING")) {
        return;
    }
    let [calls, micros, links, blobs] = STAGING.each_ref().map(|n| n.load(Ordering::Relaxed));
    crate::snapshot::diag::line_fmt(format_args!(
        "PERF-STAGE staging {:.3}s calls={calls} tree_links={links} blobs={blobs}",
        micros as f64 / 1e6
    ));
    let [setup, prepare, lock, check, hits, misses] =
        CHECKS.each_ref().map(|n| n.load(Ordering::Relaxed));
    let [stamped, walked, written] = tree_stamps::COUNTS
        .each_ref()
        .map(|n| n.load(Ordering::Relaxed));
    let seconds = |micros: u64| micros as f64 / 1e6;
    crate::snapshot::diag::line_fmt(format_args!(
        "PERF-STAGE cache-check setup={:.3}s prepare={:.3}s lock={:.3}s validate={:.3}s hits={hits} misses={misses} trees: stamped={stamped} walked={walked} stamps_written={written}",
        seconds(setup),
        seconds(prepare),
        seconds(lock),
        seconds(check),
    ));
}

/// Summed wall time (microseconds, over all workers) of an action's cache
/// check, reported under `BAZEL_RS_TIMING` ([`report_staging`]): setting
/// up the cache directories, computing the key ([`prepare`]), taking the
/// entry's lock, validating the entry ([`cache_valid`]); then the number
/// of hits and misses.
static CHECKS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

fn count_check(slot: usize, clock: &mut std::time::Instant) {
    let now = std::time::Instant::now();
    CHECKS[slot].fetch_add(
        u64::try_from(now.duration_since(*clock).as_micros()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
    *clock = now;
}

/// Stage one input file at `destination`: a file of a published repository
/// tree as a link to it ([`stage_tree_link`]), any other file as its store
/// blob's hardlink ([`stage_file`]).
fn stage_input_file<'a>(
    cas: &Path,
    root: &Path,
    input: &'a Input,
    execroot: &Path,
    destination: &Path,
    staged: &mut Vec<StagedFile>,
    distinct: &mut DistinctInodes<'a>,
) -> Result<(), String> {
    if repository_tree_file(root, &input.path) {
        STAGING[2].fetch_add(1, Ordering::Relaxed);
        stage_tree_link(input, execroot, destination, staged)
    } else {
        STAGING[3].fetch_add(1, Ordering::Relaxed);
        stage_file(cas, root, input, execroot, destination, staged, distinct)
    }
}

/// Stage every input: files of published repository trees as links to
/// them ([`stage_tree_link`]), as Bazel's runfiles trees and symlinked
/// sandboxes present `external/<repo>` files; every other file as a store
/// hardlink, one inode per distinct artifact ([`DistinctInodes`]); tree
/// directories as real (writable) directories holding their
/// files staged so. A source directory is staged like a tree, its dangling
/// symlinks as symlinks with their link text (docs/SOURCE_DIRECTORIES.md).
fn stage_inputs(
    cas: &Path,
    root: &Path,
    inputs: &BTreeMap<PathBuf, Staged>,
    execroot: &Path,
) -> Result<Vec<StagedFile>, String> {
    let clock = std::time::Instant::now();
    let staged = stage_inputs_untimed(cas, root, inputs, execroot);
    STAGING[0].fetch_add(1, Ordering::Relaxed);
    STAGING[1].fetch_add(
        u64::try_from(clock.elapsed().as_micros()).unwrap_or(u64::MAX),
        Ordering::Relaxed,
    );
    staged
}

fn stage_inputs_untimed(
    cas: &Path,
    root: &Path,
    inputs: &BTreeMap<PathBuf, Staged>,
    execroot: &Path,
) -> Result<Vec<StagedFile>, String> {
    let mut staged = Vec::new();
    let mut distinct = DistinctInodes::default();
    for (name, input) in inputs {
        match input {
            Staged::File(input) => {
                materialize(cas, root, &input.path)?;
                stage_input_file(cas, root, input, execroot, name, &mut staged, &mut distinct)?
            }
            Staged::Tree(tree) => {
                ensure_directory(&execroot.join(name))?;
                for directory in &tree.directories {
                    ensure_directory(&execroot.join(name).join(directory))?;
                }
                for (file, input) in &tree.files {
                    let destination = name.join(file);
                    stage_input_file(
                        cas,
                        root,
                        input,
                        execroot,
                        &destination,
                        &mut staged,
                        &mut distinct,
                    )?;
                }
            }
            Staged::SourceDirectory(directory) => {
                ensure_directory(&execroot.join(name))?;
                for child in &directory.directories {
                    ensure_directory(&execroot.join(name).join(child))?;
                }
                for (file, input) in &directory.files {
                    let destination = name.join(file);
                    stage_input_file(
                        cas,
                        root,
                        input,
                        execroot,
                        &destination,
                        &mut staged,
                        &mut distinct,
                    )?;
                }
                for (link, target) in &directory.dangling {
                    let path = execroot.join(name).join(link);
                    std::os::unix::fs::symlink(target, &path)
                        .map_err(|e| io_error("cannot create symbolic link", &path, e))?;
                }
            }
            // SandboxHelpers.processInputFiles: an unresolved symlink input
            // is a symlink with its literal text (SandboxInputs.symlinks);
            // unsandboxed, the execution root holds the artifact itself.
            Staged::Symlink(text) => {
                let path = execroot.join(name);
                if let Some(parent) = path.parent() {
                    ensure_directory(parent)?;
                }
                std::os::unix::fs::symlink(text, &path)
                    .map_err(|e| io_error("cannot create symbolic link", &path, e))?;
            }
        }
    }
    Ok(staged)
}

/// After a process ran: every staged inode (reached through its execution
/// root path, or the store when the process replaced that path) must be
/// unchanged. A damaged blob is removed so no later action stages it. A
/// link into a repository tree is judged by the tree's file alone (the
/// link in the execution root is the process's own to replace): the same
/// inode must be unchanged, and another inode at that path or none — a
/// file renamed over it, a removal — counts as modified as well, unless it
/// is a regular file that still holds the keyed content and mode (another
/// process's republication of the same tree identity replaces every file
/// by identical bytes and modes).
fn check_staged(staged: &[StagedFile], execroot: &Path) -> Result<(), String> {
    let mut modified = Vec::new();
    for file in staged {
        let state = |path: &Path| {
            fs::symlink_metadata(path)
                .ok()
                .map(|metadata| inode_state(&metadata))
                .filter(|state| state.inode == file.state.inode)
        };
        let changed = match file.tree_link {
            Some(digest) => match state(&file.blob) {
                Some(current) => current != file.state,
                None => !holds_keyed(&file.blob, digest, file.state.mode),
            },
            None => {
                let current =
                    state(&execroot.join(&file.destination)).or_else(|| state(&file.blob));
                let changed = current.is_some_and(|current| current != file.state);
                if changed && state(&file.blob).is_some() {
                    forget_produced(&file.blob);
                    let _ = fs::remove_file(&file.blob);
                }
                changed
            }
        };
        if changed {
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

/// Whether `path` is a regular file with permission bits `mode` whose
/// content is `digest`, hashed by its change-time key like a tree link's
/// file at staging ([`stage_tree_link`]).
fn holds_keyed(path: &Path, digest: Sha256Digest, mode: u32) -> bool {
    fs::symlink_metadata(path).is_ok_and(|metadata| {
        metadata.is_file()
            && metadata.permissions().mode() & 0o7777 == mode
            && hash_file_from(path, metadata, false, false).is_ok_and(|(found, _)| found == digest)
    })
}

/// Publish an execution-root output file as `target`: normalized to
/// OUTPUT_MODE (OutputPermissions.READONLY) and linked to its store blob, so
/// identical outputs share one inode. A failed hardlink falls back to a
/// rename (SandboxHelpers.moveOutputs, :178-236), then a copy.
fn publish_file(cas: &Path, root: &Path, source: &Path, target: &Path) -> Result<Input, String> {
    // The execution root is the process's: always checked in full.
    regular_file(source, None)?;
    set_mode(source, OUTPUT_MODE)?;
    let metadata =
        fs::symlink_metadata(source).map_err(|e| io_error("cannot inspect", source, e))?;
    let (input, state) = read_regular_state(source, metadata, true, true)?;
    if let Some(parent) = target.parent() {
        ensure_directory(parent)?;
    }
    let blob = ensure_blob_hashed(cas, &input, root, Some(state))?;
    if hard_link(&blob, target).is_ok() || fs::rename(source, target).is_ok() {
        count_link(true);
    } else {
        link_ensured_blob(cas, root, &input, target, OUTPUT_MODE)?;
    }
    Ok(input)
}

/// Publish an execution-root tree output, read by [`read_tree`]
/// ([`TreeRead::Output`]) while its execution root was intact, as `target`
/// by renaming it, then replace each file by its store blob's inode and make
/// directories read-only deepest first, the root TREE_ROOT_MODE. A symlink
/// in the tree is replaced by what it resolved to ([`materialize_links`]):
/// genuine keeps the links in its one output tree, where a relative link
/// resolves as it did in the execution root, but a native output lives in
/// its action's own cache entry, where it would not (docs/TREE_SYMLINKS.md).
fn publish_tree(
    cas: &Path,
    root: &Path,
    source: &Path,
    target: &Path,
    read: ReadTree,
) -> Result<Tree, String> {
    let ReadTree {
        tree,
        linked,
        states,
    } = read;
    if let Some(parent) = target.parent() {
        ensure_directory(parent)?;
    }
    fs::rename(source, target).map_err(|e| io_error("cannot publish tree", target, e))?;
    count_link(true);
    if !tree.links.is_empty() {
        materialize_links(target, &tree, &linked)?;
    }
    for (name, input) in &tree.files {
        if through_link(&tree.links, name) {
            continue;
        }
        let path = target.join(name);
        let input = Input {
            path: path.clone(),
            ..input.clone()
        };
        // Renamed with its tree: still the inode in the state it was read in.
        let blob = ensure_blob_hashed(cas, &input, root, states.get(name).copied())?;
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

/// A tree output read before anything of its action is published
/// ([`TreeRead::Output`]), with the store blobs of the files its links led
/// to (name, blob, blob mode: the target's executable bits, as genuine shows
/// the target's mode through the link), ensured while every link still
/// resolves (a link can lead into another output, which publication moves),
/// and the state each of its own (normalized) files was read in
/// ([`read_walked`], for [`ensure_blob_hashed`]).
struct ReadTree {
    tree: Tree,
    linked: Vec<(PathBuf, PathBuf, u32)>,
    states: BTreeMap<PathBuf, FileKey>,
}

fn read_output_tree(
    cas: &Path,
    root: &Path,
    tree: Tree,
    states: BTreeMap<PathBuf, FileKey>,
) -> Result<ReadTree, String> {
    let mut linked = Vec::new();
    for (name, input) in &tree.files {
        if through_link(&tree.links, name) {
            linked.push((
                name.clone(),
                ensure_blob(cas, input, root)?,
                blob_mode(input),
            ));
        }
    }
    Ok(ReadTree {
        tree,
        linked,
        states,
    })
}

/// Replace the symlinks of a tree just published at `target` by what they
/// resolved to: a file by its blob ([`ReadTree::linked`]), a directory by a
/// real directory holding what was found below the link. Directories no
/// link leads to are made writable first (an action can leave one
/// read-only; [`publish_tree`] makes every directory OUTPUT_MODE
/// afterwards); nothing is changed through a link.
fn materialize_links(
    target: &Path,
    tree: &Tree,
    linked: &[(PathBuf, PathBuf, u32)],
) -> Result<(), String> {
    set_mode(target, 0o755)?;
    for directory in &tree.directories {
        if !through_link(&tree.links, directory) {
            set_mode(&target.join(directory), 0o755)?;
        }
    }
    for link in &tree.links {
        // Only the outermost links: one below a linked directory is gone
        // with it.
        if link
            .parent()
            .is_some_and(|parent| through_link(&tree.links, parent))
        {
            continue;
        }
        let path = target.join(link);
        fs::remove_file(&path).map_err(|e| io_error("cannot replace symlink", &path, e))?;
    }
    // Component order: a directory before the directories below it.
    for directory in &tree.directories {
        if through_link(&tree.links, directory) {
            let path = target.join(directory);
            fs::create_dir(&path).map_err(|e| io_error("cannot create directory", &path, e))?;
        }
    }
    for (name, blob, mode) in linked {
        link_or_copy(blob, &target.join(name), *mode)?;
    }
    Ok(())
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
        modes: tree
            .files
            .iter()
            .filter(|(_, input)| blob_mode(input) != OUTPUT_MODE)
            .map(|(name, input)| (hex(name.as_os_str().as_bytes()), blob_mode(input)))
            .collect(),
    }
}

/// A published tree (or a symlink output's target tree) with completed-tree
/// permissions and the manifest's contents: every file exactly its blob's
/// mode (OUTPUT_MODE, or the manifest's mode of a file a link led to),
/// every directory OUTPUT_MODE.
fn tree_valid(path: &Path, expected: &TreeManifest, root: &Path) -> Result<Option<Tree>, String> {
    if real_directory(path, Some(root))?.permissions().mode() & 0o7777 != TREE_ROOT_MODE {
        return Ok(None);
    }
    let mode = if published_output(Some(root), path) {
        TreeRead::Published
    } else {
        TreeRead::Input
    };
    let tree = read_tree(path, Some(root), mode)?;
    if tree_manifest(&tree) != *expected
        || tree
            .files
            .values()
            .any(|input| input.mode != blob_mode(input))
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

/// Tree outputs' listings by output name.
type Listings = BTreeMap<PathBuf, Vec<PathBuf>>;

/// The tree listings of a valid cache entry, and whether its tree stamps
/// changed (crate::action::tree_stamps; recorded only when
/// `write_stamps`: the caller holds the entry's lock). Every validated
/// published tree (also a symlink output's target tree) seeds the tree
/// memo, and every other published output the output memo, so consumers in
/// this process never inspect them again ([`read_staged`]).
///
/// A remote-only output (Build without the Bytes) without a file is valid
/// while `stay_remote` admits it and its metadata has not expired
/// (`--experimental_remote_cache_ttl`); otherwise the action reruns, which
/// downloads it (ActionCacheChecker with RemoteOutputChecker
/// .shouldTrustMetadata). Once materialized it is an ordinary output.
fn cache_valid(
    directory: &Path,
    key: &str,
    outputs: &[PathBuf],
    (trees, links): (&BTreeSet<PathBuf>, &BTreeSet<PathBuf>),
    root: &Path,
    write_stamps: bool,
    stay_remote: &dyn Fn(&Path) -> bool,
) -> Option<(Listings, bool)> {
    let manifest_path = directory.join("manifest.json");
    let mut validated = Vec::new();
    let mut values = Vec::new();
    let mut remote_values = Vec::new();
    let mut check = || -> Result<Option<(Listings, bool)>, String> {
        let bytes = read_manifest(&manifest_path, root)?;
        let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if manifest.format != MANIFEST_FORMAT
            || manifest.key != key
            || manifest.outputs.len() != outputs.len()
        {
            return Ok(None);
        }
        let mut listings = BTreeMap::new();
        let mut stamps = tree_stamps::EntryStamps::new(directory, &bytes);
        for (index, (entry, name)) in manifest.outputs.iter().zip(outputs).enumerate() {
            if entry.name != hex(name.as_os_str().as_bytes()) {
                return Ok(None);
            }
            let mut path = directory.join("outputs").join(name);
            // An unresolved symlink output is still a symlink with the
            // recorded link text, whose value the digest is.
            if links.contains(name) {
                let Some(target) = &entry.symlink else {
                    return Ok(None);
                };
                let text = read_unresolved_symlink(&path)?;
                let text = text.as_os_str().as_bytes();
                if hex(text) != *target
                    || !unresolved_symlink_digest(text).spelled(&entry.digest)
                    || entry.tree.is_some()
                {
                    return Ok(None);
                }
                values.push((
                    path,
                    PublishedValue::Link(PathBuf::from(OsStr::from_bytes(text))),
                ));
                continue;
            }
            let published = path.clone();
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
                    && !runfiles_tree_valid(&directory.join("outputs"), name, tree, root, |dir| {
                        stamps.runfiles(index, dir, tree, root, || {
                            runfiles_listing(dir, Some(root)).ok().as_ref() == Some(&tree.entries)
                        })
                    })
                {
                    return Ok(None);
                }
                path = resolve_published(&path, Some(root))?.0;
            }
            match (&entry.tree, trees.contains(name)) {
                (Some(expected), true) => {
                    // A published tree (also a symlink output's target) by
                    // its directory stamps; any other tree in full.
                    let tree = if published_output(Some(root), &path) {
                        stamps.tree(index, &path, expected, root)?
                    } else {
                        tree_valid(&path, expected, root)?
                    };
                    let Some(tree) = tree else {
                        return Ok(None);
                    };
                    listings.insert(name.clone(), tree_listing(&tree));
                    validated.push((path, tree));
                }
                (None, false)
                    if entry.remote.is_some()
                        && entry.symlink.is_none()
                        && fs::symlink_metadata(&path)
                            .is_err_and(|error| error.kind() == io::ErrorKind::NotFound) =>
                {
                    let remote = entry.remote.as_ref().expect("checked");
                    if !stay_remote(name) || !crate::remote_outputs::alive(remote.expires) {
                        return Ok(None);
                    }
                    remote_values.push((
                        path,
                        name.clone(),
                        crate::remote_outputs::RemoteFile {
                            hash: entry.digest.clone(),
                            size: remote.size,
                        },
                    ));
                }
                (None, false) => {
                    // read_input (no symlink to follow), with the file's
                    // identity for the output tree.
                    let (actual, identity) = if store_file(Some(root), &path) {
                        (read_input(&path, Some(root), false)?, None)
                    } else {
                        let metadata = regular_file(&path, Some(root))?;
                        let identity = (metadata.dev(), metadata.ino());
                        let owned = owned_by(Some(root), &path);
                        (
                            read_regular_from(&path, metadata, owned, false)?,
                            Some(identity),
                        )
                    };
                    if actual.mode != entry.mode || !actual.digest.spelled(&entry.digest) {
                        return Ok(None);
                    }
                    // What a consumer reads through the published symlinks:
                    // a published output, or a file of the immutable stores
                    // (a repository tree's file, which the file memo keeps
                    // by its path for the whole process anyway); a source
                    // keeps the checks of its own area on every use.
                    if published_output(Some(root), &actual.path)
                        || store_file(Some(root), &actual.path)
                    {
                        let identity = identity.filter(|_| actual.path == published);
                        values.push((published, PublishedValue::File(actual, identity)));
                    }
                }
                _ => return Ok(None),
            }
        }
        let pending = stamps.finish(write_stamps);
        Ok(Some((listings, pending)))
    };
    let (listings, pending) = check().unwrap_or(None)?;
    for (path, tree) in validated {
        if published_output(Some(root), &path) {
            remember_tree(path, Arc::new(tree));
        }
    }
    remember_outputs(root, values);
    for (path, exec, file) in remote_values {
        crate::remote_outputs::register(path, exec, file);
    }
    Some((listings, pending))
}

/// The bytes of an entry's manifest: a regular file (never through a
/// symlink, also not at the leaf) with no linked ancestor, opened once and
/// inspected through its descriptor.
fn read_manifest(path: &Path, root: &Path) -> Result<Vec<u8>, String> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)
        .map_err(|e| io_error("cannot read manifest", path, e))?;
    let metadata = file
        .metadata()
        .map_err(|e| io_error("cannot inspect", path, e))?;
    let metadata = checked_path(path, Some(root), Some(metadata))?;
    if !metadata.is_file() {
        return Err(format!("expected regular file: '{}'", path.display()));
    }
    // The entry's last use, for the cache's garbage collection (at most
    // once per day per entry; docs/CACHE_GC.md).
    crate::cache_gc::note_used(path, &metadata);
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0) + 1);
    file.read_to_end(&mut bytes)
        .map_err(|e| io_error("cannot read manifest", path, e))?;
    Ok(bytes)
}

/// The TMPDIR every local spawn runs with, standalone and sandboxed alike:
/// the client's `TMPDIR`, or `/tmp` when it is unset or empty
/// (PosixLocalEnvProvider.rewriteLocalEnv,
/// exec/local/PosixLocalEnvProvider.java:41-59, which every local runner —
/// StandaloneSpawnStrategy, LinuxSandboxedSpawnRunner:269-270,
/// ProcessWrapperSandboxedSpawnRunner:76-77 — applies). The short `/tmp`
/// fallback is deliberate (bazel#4376): tools bind Unix sockets below TMPDIR
/// and `sun_path` holds ~108 bytes, which a per-action directory below the
/// cache would exceed. The value is used as given (a relative one resolves
/// from the action's working directory, as under Bazel); the client
/// environment is this process's environment.
pub(crate) fn local_tmpdir() -> PathBuf {
    std::env::var_os("TMPDIR")
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
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
            match result {
                // Another thread removed it first (two actions validating
                // one shared blob): the entry is gone either way.
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                result => {
                    result.map_err(|e| io_error("cannot remove invalid cache entry", path, e))
                }
            }
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
    let origin = |destination: &Path| -> Result<&Path, String> {
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
        Transform::WorkspaceStatus { .. } => Err(
            "internal error: a workspace status placeholder must be expanded before execution"
                .to_owned(),
        ),
        Transform::Unsupported { message } => Err(message.clone()),
        // UnresolvedSymlinkAction.execute: outputPath.createSymbolicLink(
        // PathFragment.create(target), targetType).
        Transform::UnresolvedSymlink {
            target, progress, ..
        } => std::os::unix::fs::symlink(unresolved_link_text(target), &output).map_err(|e| {
            format!(
                "{progress} failed: failed to create symbolic link '{}' with target '{target}' due to I/O error: {}",
                action.outputs[0].display(),
                io_error("cannot create symbolic link", &output, e)
            )
        }),
        Transform::RunfilesManifest { entries } => {
            let mut sorted: Vec<&(std::sync::Arc<str>, Option<SharedPath>)> = entries.iter().collect();
            sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            let mut text = Vec::new();
            for (path, destination) in sorted {
                let target = match destination {
                    // An unresolved symlink's line names its link text
                    // (SourceManifestAction.writeFile :301-312).
                    Some(destination) => Some(match inputs.get(destination.as_path()) {
                        Some(Staged::Symlink(link)) => link.as_path(),
                        _ => origin(destination)?,
                    })
                    .map(|target| {
                        target.to_str().ok_or_else(|| {
                            format!(
                                "runfiles target '{}' is not valid UTF-8",
                                destination.display()
                            )
                        })
                    })
                    .transpose()?,
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
                for (path, destination) in entries.iter() {
                    if &**path == "MANIFEST" {
                        // linkManifest replaces it (SymlinkTreeHelper.java:167-176).
                        continue;
                    }
                    if path.starts_with("MANIFEST/") {
                        return Err(format!(
                            "runfiles entry '{path}' is below the runfiles MANIFEST"
                        ));
                    }
                    let link = directory.join(&**path);
                    if let Some(parent) = link.parent() {
                        ensure_directory(parent)?;
                    }
                    match destination {
                        // Unresolved symlinks are created textually
                        // (SymlinkTreeHelper.createRunfilesSymlinks).
                        Some(destination) => {
                            let target = match inputs.get(destination.as_path()) {
                                Some(Staged::Symlink(text)) => text.as_path(),
                                _ => origin(destination)?,
                            };
                            std::os::unix::fs::symlink(target, &link)
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
    entries_valid: impl FnOnce(&Path) -> bool,
) -> bool {
    let relative = manifest.parent().unwrap_or(Path::new(""));
    // The listing (runfiles_listing), or its directory stamps.
    if !entries_valid(&outputs.join(relative)) {
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
/// A regular file (or a source directory) read by content for a process
/// outside the action graph (a test action, crate::test_runner, N23g): its
/// digest and executable bits, staged like any action input.
#[derive(Clone)]
pub struct ContentInput(Staged);

impl ContentInput {
    /// The SHA-256 in lowercase hex: a file's content digest, a source
    /// directory's framed listing (the action key's framing of it).
    pub fn digest(&self) -> String {
        match &self.0 {
            Staged::File(input) => input.digest.hex(),
            staged => {
                let mut hash = Sha256::new();
                frame_staged(&mut hash, staged);
                hex(&hash.finalize())
            }
        }
    }

    pub fn executable(&self) -> bool {
        match &self.0 {
            Staged::File(input) => input.executable != 0,
            _ => false,
        }
    }

    /// Adds this input at `destination` to a remote spawn's input root
    /// (crate::remote_cache::SpawnSpec, MerkleTreeComputer as for
    /// [`remote_action`]'s inputs) and where its blobs' bytes are.
    pub fn add_remote<'a>(
        &'a self,
        destination: &str,
        spec: &mut crate::remote_cache::SpawnSpec,
        blobs: &mut BTreeMap<crate::reapi::Digest, crate::remote_cache::BlobSource<'a>>,
    ) -> Result<(), String> {
        let mut add = |path: String, input: &'a Input| {
            let digest = crate::reapi::Digest {
                hash: input.digest.hex(),
                size: input.size,
            };
            blobs
                .entry(digest.clone())
                .or_insert(crate::remote_cache::BlobSource::File(input.path.as_path()));
            spec.inputs.insert(path, digest);
        };
        let join = |name: &Path| -> Result<String, String> {
            crate::remote_cache::path_string(&Path::new(destination).join(name))
        };
        match &self.0 {
            Staged::File(input) => add(destination.to_owned(), input),
            Staged::Tree(tree) => {
                if tree.files.is_empty() {
                    spec.directories.insert(destination.to_owned());
                }
                for (name, input) in &tree.files {
                    add(join(name)?, input);
                }
            }
            Staged::SourceDirectory(directory) => {
                if let Some(link) = directory.dangling.keys().next() {
                    return Err(format!(
                        "The file type of '{}' is not supported.",
                        Path::new(destination).join(link).display()
                    ));
                }
                if directory.files.is_empty() {
                    spec.directories.insert(destination.to_owned());
                }
                for (name, input) in &directory.files {
                    add(join(name)?, input);
                }
            }
            Staged::Symlink(text) => {
                let text = text
                    .to_str()
                    .ok_or_else(|| format!("non-UTF-8 symlink target of '{destination}'"))?;
                spec.symlinks
                    .insert(destination.to_owned(), text.to_owned());
            }
        }
        Ok(())
    }
}

/// The canonical cache root, created if needed, and its store directory.
fn cache_store(cache_root: &Path) -> Result<(PathBuf, PathBuf), String> {
    let directories = ensure_cache(cache_root)?;
    Ok((directories.root.clone(), directories.cas.clone()))
}

/// The canonical cache root and the directories every action uses below it
/// ([`ensure_cache`]).
struct CacheDirectories {
    root: PathBuf,
    actions: PathBuf,
    locks: PathBuf,
    work: PathBuf,
    cas: PathBuf,
}

/// Cache roots (as the caller spells them) whose directories this process
/// created or found to be real directories, with their canonical paths.
fn ensured_caches() -> &'static Mutex<HashMap<PathBuf, Arc<CacheDirectories>>> {
    static MEMO: OnceLock<Mutex<HashMap<PathBuf, Arc<CacheDirectories>>>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The directories of `cache_root` when this process already ensured them.
fn ensured_cache(cache_root: &Path) -> Option<Arc<CacheDirectories>> {
    ensured_caches()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(cache_root)
        .cloned()
}

/// Forget that `cache_root`'s directories exist (one of them was found
/// missing): the next [`ensure_cache`] creates them again.
fn forget_ensured_cache(cache_root: &Path) {
    ensured_caches()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(cache_root);
}

/// Create the cache root and its `actions/`, `locks/`, `work/` and `cas/`
/// directories (each must be a real directory) once per process and cache
/// root, instead of once per action: a warm build checks tens of thousands
/// of actions, and `create_dir_all` plus `canonicalize` (a `readlink` per
/// path component) for each cost more than most cache checks. Nothing
/// bazel-rs runs removes them while a command runs; a test that removes a
/// cache sees [`forget_ensured_cache`] (the lock of the next action is
/// missing) and the directories created again.
fn ensure_cache(cache_root: &Path) -> Result<Arc<CacheDirectories>, String> {
    if let Some(directories) = ensured_cache(cache_root) {
        return Ok(directories);
    }
    fs::create_dir_all(cache_root).map_err(|e| io_error("cannot create cache", cache_root, e))?;
    let root = fs::canonicalize(cache_root)
        .map_err(|e| io_error("cannot resolve cache", cache_root, e))?;
    let directories = CacheDirectories {
        actions: root.join("actions"),
        locks: root.join("locks"),
        work: root.join("work"),
        cas: root.join(CAS),
        root,
    };
    for path in [
        &directories.actions,
        &directories.locks,
        &directories.work,
        &directories.cas,
    ] {
        ensure_directory(path)?;
    }
    let directories = Arc::new(directories);
    ensured_caches()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(cache_root.to_path_buf(), Arc::clone(&directories));
    Ok(directories)
}

/// Open (creating it) an action's lock file, never through a symlink.
fn open_lock(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
}

/// Whether opening a lock failed because something other than a regular
/// file is at its path: a symlink (ELOOP under `O_NOFOLLOW`) or a directory.
fn not_a_lock_file(error: &io::Error) -> bool {
    error.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error())
        || error.kind() == io::ErrorKind::IsADirectory
}

/// Read `path` (a source file or a published output; published symlink
/// outputs are followed) by content, like an action input (prepare). A
/// source directory (a real directory outside the cache's published outputs,
/// a runfile like Bazel's runfiles link to it) is read like a source
/// directory input.
pub fn read_content_input(cache_root: &Path, path: &Path) -> Result<ContentInput, String> {
    let (root, _) = cache_store(cache_root)?;
    let directory = !published_output(Some(&root), path)
        && fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir());
    if directory {
        return Ok(ContentInput(Staged::SourceDirectory(source_directory(
            path,
            Some(&root),
        )?)));
    }
    read_input(path, Some(&root), true).map(|input| ContentInput(Staged::File(input)))
}

/// An unresolved symlink artifact's published symlink as a content input:
/// its link text, staged as a symlink with that text (a test's runfiles,
/// SymlinkTreeHelper.createRunfilesSymlinks).
pub fn read_symlink_input(path: &Path) -> Result<ContentInput, String> {
    Ok(ContentInput(Staged::Symlink(read_unresolved_symlink(
        path,
    )?)))
}

/// A runfile's source read by content ([`read_content_source`]).
pub enum ContentSource {
    File(ContentInput),
    /// A tree artifact: its files by their paths below the tree (none for
    /// an empty tree).
    Tree(Vec<(PathBuf, ContentInput)>),
}

/// [`read_content_input`] for a runfile that may be a tree artifact (or
/// another directory): a directory is read like a tree input (a published
/// tree from the tree memo) and taken as its files, as a spawn's runfiles
/// expand a tree into its children (SpawnInputExpander
/// .addSingleRunfilesTreeToInputs, exec/SpawnInputExpander.java:87-132).
pub fn read_content_source(cache_root: &Path, path: &Path) -> Result<ContentSource, String> {
    let (root, _) = cache_store(cache_root)?;
    // A source directory is one staged directory (docs/SOURCE_DIRECTORIES.md).
    if !published_output(Some(&root), path)
        && fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
    {
        return read_content_input(cache_root, path).map(ContentSource::File);
    }
    let (resolved, _) = resolve_published(path, Some(&root))?;
    if !fs::symlink_metadata(&resolved).is_ok_and(|metadata| metadata.is_dir()) {
        return read_input(path, Some(&root), true)
            .map(|input| ContentSource::File(ContentInput(Staged::File(input))));
    }
    match read_staged(path, InputKind::Tree, Some(&root))? {
        Staged::Tree(tree) => Ok(ContentSource::Tree(
            tree.files
                .iter()
                .map(|(name, input)| (name.clone(), ContentInput(Staged::File(input.clone()))))
                .collect(),
        )),
        staged => Ok(ContentSource::File(ContentInput(staged))),
    }
}

/// Stage content inputs below `execroot` at their relative destinations:
/// read-only hardlinks of the cache's content-addressed store (N36), like
/// stage_inputs for an action (a source directory as a real directory).
pub fn stage_content_inputs(
    cache_root: &Path,
    inputs: &[(ContentInput, PathBuf)],
    execroot: &Path,
) -> Result<(), String> {
    let (root, cas) = cache_store(cache_root)?;
    let mut staged = BTreeMap::new();
    for (input, destination) in inputs {
        validate_relative(destination)?;
        staged.insert(destination.clone(), input.0.clone());
    }
    stage_inputs(&cas, &root, &staged, execroot)?;
    Ok(())
}

pub fn execute(cache_root: &Path, action: &Action) -> Result<ActionResult, String> {
    execute_with_remote(cache_root, action, crate::remote_cache::active())
}

/// Log one executed spawn (crate::exec_log): its argv, environment, staged
/// inputs (sorted by exec path, directories expanded), declared and actual
/// outputs (read from the execution root before publication), runner and
/// exit code.
#[allow(clippy::too_many_arguments)]
fn log_spawn(
    action: &Action,
    prepared: &PreparedAction,
    execroot: &Path,
    runner: &str,
    exit_code: i32,
    cache_hit: bool,
    (start, wall): (SystemTime, std::time::Duration),
    key: Option<&crate::reapi::Digest>,
) {
    use crate::exec_log::LogFile;
    let text = |path: &Path| path.display().to_string();
    // The inputs in the action's declaration order (the compact log's input
    // set order; the expanded log sorts them).
    let mut inputs: Vec<LogFile> = Vec::new();
    let file = |path: String, input: &Input, directory: Option<String>| LogFile {
        path,
        digest: Some((input.digest.hex(), input.size)),
        symlink_target: None,
        directory,
    };
    let mut seen: BTreeSet<&Path> = BTreeSet::new();
    let declared = action
        .inputs
        .iter()
        .map(|(_, destination)| destination.as_path())
        .chain(prepared.inputs.keys().map(PathBuf::as_path));
    for path in declared {
        let Some(staged) = prepared.inputs.get(path) else {
            continue;
        };
        if !seen.insert(path) {
            continue;
        }
        let exec = text(path);
        match staged {
            Staged::File(input) => inputs.push(file(exec, input, None)),
            Staged::Tree(tree) => {
                for (name, input) in &tree.files {
                    inputs.push(file(text(&path.join(name)), input, Some(exec.clone())));
                }
            }
            Staged::SourceDirectory(directory) => {
                for (name, input) in &directory.files {
                    inputs.push(file(text(&path.join(name)), input, Some(exec.clone())));
                }
            }
            Staged::Symlink(target) => inputs.push(LogFile {
                path: exec,
                digest: None,
                symlink_target: Some(text(target)),
                directory: None,
            }),
        }
    }
    for (path, content) in &action.extra.param_files {
        let digest = digest(content);
        inputs.push(LogFile {
            path: text(path),
            digest: Some((digest.hex(), content.len() as u64)),
            symlink_target: None,
            directory: None,
        });
    }
    let has = |key: &str| {
        action.execution_info.contains(key) || action.extra.execution_info.contains_key(key)
    };
    let (remote_cacheable, remotable) = remote_policy(action);
    crate::exec_log::log(crate::exec_log::Spawn {
        args: arguments(action),
        environment: resolved_environment(action)
            .into_iter()
            .map(|(name, value)| (name, value.to_string_lossy().into_owned()))
            .collect(),
        inputs,
        listed_outputs: action.outputs.iter().map(|path| text(path)).collect(),
        actual_outputs: action
            .outputs
            .iter()
            .map(|name| {
                let exec = text(name);
                let files = crate::exec_log::output_files(&exec, &execroot.join(name));
                (exec, files)
            })
            .collect(),
        remotable,
        cacheable: !has("no-cache") && !has("local"),
        remote_cacheable,
        mnemonic: action.extra.mnemonic.clone(),
        runner: runner.to_owned(),
        cache_hit,
        status: if exit_code == 0 {
            String::new()
        } else {
            "NON_ZERO_EXIT".to_owned()
        },
        exit_code,
        target_label: action.id.split('#').next().unwrap_or(&action.id).to_owned(),
        digest: key.map(|key| (key.hash.clone(), key.size)),
        start,
        wall,
    });
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
    let state =
        fs::symlink_metadata(&candidate).map_err(|e| io_error("cannot inspect", &candidate, e))?;
    if store_candidate(&candidate, &blob, || blob_valid(&blob, &input))? {
        // The bytes this process wrote.
        remember_created(&blob, &state, input.digest);
    }
    Ok(blob)
}

/// The content-addressed store of this invocation's remote downloads
/// (set once a spawn may use the remote cache): a blob already stored is
/// never fetched again, and a fetched blob is stored at once, so the same
/// digest in several outputs (the same file in two configurations' tree
/// artifacts, an output fetched again for a later consumer) crosses the
/// network once — Bazel's CombinedCache deduplicates in-flight downloads
/// (RemoteCacheClient's `downloadsInProgress`) and its output filesystem
/// keeps fetched files.
static DOWNLOAD_STORE: OnceLock<PathBuf> = OnceLock::new();

fn note_download_store(cache_root: &Path) {
    if DOWNLOAD_STORE.get().is_none() {
        let _ = DOWNLOAD_STORE.set(cache_root.join("cas"));
    }
}

/// The stored bytes of blob `hash` (`size` bytes) when this invocation's
/// store holds it (either mode); the caller verifies them.
pub(crate) fn stored_blob(hash: &str, size: u64) -> Option<Vec<u8>> {
    let cas = DOWNLOAD_STORE.get()?;
    let digest = Sha256Digest::from_hex(hash)?;
    for mode in [0o444, 0o555] {
        let blob = cas.join(format!("{digest}-{mode:o}"));
        if let Ok(metadata) = fs::symlink_metadata(&blob)
            && metadata.is_file()
            && metadata.size() == size
            && let Ok(bytes) = fs::read(&blob)
        {
            return Some(bytes);
        }
    }
    None
}

/// Stores downloaded (verified) bytes for [`stored_blob`]; best effort.
pub(crate) fn store_downloaded(bytes: &[u8]) {
    if let Some(cas) = DOWNLOAD_STORE.get() {
        let _ = store_bytes(cas, bytes, 0);
    }
}

/// `action` with its embedded inputs stored in the content-addressed store
/// `cas` and turned into ordinary inputs of their blobs (N41's
/// genrule-setup.sh, staged through N36's store like every input).
fn with_embedded_inputs(action: &Action, cas: &Path) -> Result<Action, String> {
    let mut action = action.clone();
    for (destination, bytes) in std::mem::take(&mut action.embedded_inputs) {
        let blob = store_bytes(cas, bytes, 0o111)?;
        action.inputs.push((blob.into(), destination.into()));
    }
    Ok(action)
}

/// The REAPI action of a prepared process action
/// (RemoteExecutionService.buildRemoteAction, crate::remote_cache): the argv,
/// [`resolved_environment`], the input root (every regular input file at its
/// execution path; tree inputs and source directories as their files,
/// without empty subdirectories, an empty one as an empty directory;
/// parameter files at their paths, ParamFileActionInput; every tree output
/// as an empty directory, MerkleTreeComputer's EmptyInputDirectory), the
/// declared outputs, the `Platform` and the timeout. Also returns where
/// each input blob's bytes are, for remote execution's uploads.
fn remote_action<'a>(
    action: &'a Action,
    prepared: &'a PreparedAction,
    output_paths: bool,
    platform: &[(String, String)],
) -> Result<
    (
        crate::remote_cache::RemoteAction,
        BTreeMap<crate::reapi::Digest, crate::remote_cache::BlobSource<'a>>,
    ),
    String,
> {
    use crate::remote_cache::{BlobSource, path_string};
    let file_digest = |input: &Input| crate::reapi::Digest {
        hash: input.digest.hex(),
        size: input.size,
    };
    let mut spec = crate::remote_cache::SpawnSpec::default();
    let mut blobs = BTreeMap::new();
    let mut add = |spec: &mut crate::remote_cache::SpawnSpec, path: String, input: &'a Input| {
        let digest = file_digest(input);
        blobs
            .entry(digest.clone())
            .or_insert(BlobSource::File(input.path.as_path()));
        spec.inputs.insert(path, digest);
    };
    for (path, staged) in &prepared.inputs {
        match staged {
            Staged::File(input) => add(&mut spec, path_string(path)?, input),
            Staged::Tree(tree) => {
                if tree.files.is_empty() {
                    spec.directories.insert(path_string(path)?);
                }
                for (name, input) in &tree.files {
                    add(&mut spec, path_string(&path.join(name))?, input);
                }
            }
            // MerkleTreeComputer.explodeDirectory (remote/merkletree/
            // MerkleTreeComputer.java:1113-1141): the files below a source
            // directory, symlinks followed, without empty directories; any
            // other entry (a dangling symlink) is `The file type of '...' is
            // not supported.`, which fails genuine's spawn, while native
            // runs it locally.
            Staged::SourceDirectory(directory) => {
                if let Some(link) = directory.dangling.keys().next() {
                    return Err(format!(
                        "The file type of '{}' is not supported.",
                        path.join(link).display()
                    ));
                }
                if directory.files.is_empty() {
                    spec.directories.insert(path_string(path)?);
                }
                for (name, input) in &directory.files {
                    add(&mut spec, path_string(&path.join(name))?, input);
                }
            }
            // MerkleTreeComputer (remote/merkletree/MerkleTreeComputer.java:
            // 651-662): a SymlinkNode with the link text.
            Staged::Symlink(text) => {
                let text = text
                    .to_str()
                    .ok_or_else(|| format!("non-UTF-8 symlink target of '{}'", path.display()))?;
                spec.symlinks.insert(path_string(path)?, text.to_owned());
            }
        }
    }
    for (path, content) in &action.extra.param_files {
        let digest = crate::reapi::Digest::of(content);
        blobs
            .entry(digest.clone())
            .or_insert(BlobSource::Bytes(content.as_slice()));
        spec.inputs.insert(path_string(path)?, digest);
    }
    spec.outputs = action
        .outputs
        .iter()
        .map(|path| path_string(path))
        .collect::<Result<Vec<_>, _>>()?;
    for tree in &action.extra.tree_outputs {
        let tree = path_string(tree)?;
        spec.directories.insert(tree.clone());
        spec.output_directories.insert(tree);
    }
    spec.environment = resolved_environment(action)
        .into_iter()
        .map(|(name, value)| match value.into_string() {
            Ok(value) => Ok((name, value)),
            Err(_) => Err(format!(
                "non-UTF-8 value of {name} cannot enter a remote cache key"
            )),
        })
        .collect::<Result<BTreeMap<_, _>, String>>()?;
    spec.arguments = arguments(action);
    spec.platform = platform.to_vec();
    // Spawns.getTimeout: the `timeout` execution requirement in seconds.
    spec.timeout_seconds = match action.extra.execution_info.get("timeout") {
        None => None,
        Some(text) => Some(
            text.parse::<i32>()
                .map_err(|_| "could not parse timeout".to_owned())?
                .max(0) as u64,
        ),
    };
    let (cacheable, executable) = remote_policy(action);
    spec.cacheable = cacheable;
    spec.may_be_executed_remotely = executable;
    spec.use_output_paths = output_paths;
    Ok((crate::remote_cache::remote_action_of(&spec)?, blobs))
}

/// The declared outputs of `action` for downloads.
fn declared_outputs(
    action: &Action,
) -> Result<Vec<(String, crate::remote_cache::OutputKind)>, String> {
    use crate::remote_cache::OutputKind;
    action
        .outputs
        .iter()
        .map(|path| {
            let kind = if action.extra.symlink_outputs.contains(path) {
                OutputKind::Symlink
            } else if action.extra.tree_outputs.contains(path) {
                OutputKind::Directory
            } else {
                OutputKind::File
            };
            Ok((crate::remote_cache::path_string(path)?, kind))
        })
        .collect()
}

/// How UploadManifest.addFiles (remote/UploadManifest.java:238-300) uploads
/// a symlink output: as a symlink with its text if it is relative or
/// dangling, as the file it resolves to if it is absolute (`None`: an
/// absolute symlink to a directory, uploaded as a directory, which the
/// native remote cache does not model).
fn symlink_upload(path: &Path) -> Result<Option<crate::remote_cache::Output>, String> {
    let text = fs::read_link(path).map_err(|e| io_error("cannot read link", path, e))?;
    let as_symlink = || {
        text.to_str()
            .map(|text| Some(crate::remote_cache::Output::Symlink(text.to_owned())))
            .ok_or_else(|| format!("non-UTF-8 symlink target of '{}'", path.display()))
    };
    if text.is_relative() {
        return as_symlink();
    }
    match fs::metadata(path) {
        Err(_) => as_symlink(),
        Ok(metadata) if metadata.is_file() => Ok(Some(crate::remote_cache::Output::File(
            fs::read(path).map_err(|e| io_error("cannot read output", path, e))?,
        ))),
        Ok(_) => Ok(None),
    }
}

/// How UploadManifest.DirectoryBuilder (remote/UploadManifest.java:
/// 470-548) uploads a tree artifact output: regular files with their
/// bytes, symlinks that are relative or dangling as symlinks, absolute ones
/// as the file or directory they resolve to, and every directory (empty
/// ones included). `None`: a special file, which genuine rejects
/// (`ILLEGAL_OUTPUT`) and the native profile keeps off the cache.
pub(crate) fn tree_upload(
    root: &Path,
) -> Result<Option<crate::remote_cache::DownloadedTree>, String> {
    fn visit(
        path: &Path,
        relative: &str,
        tree: &mut crate::remote_cache::DownloadedTree,
        depth: usize,
    ) -> Result<bool, String> {
        if depth > 4096 {
            return Err(format!("tree artifact {} is too deep", path.display()));
        }
        let entries = fs::read_dir(path).map_err(|e| io_error("cannot read directory", path, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| io_error("cannot read directory", path, e))?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| format!("non-UTF-8 file name in '{}'", path.display()))?;
            let child = entry.path();
            let child_relative = if relative.is_empty() {
                name.to_owned()
            } else {
                format!("{relative}/{name}")
            };
            let mut kind = entry
                .file_type()
                .map_err(|e| io_error("cannot inspect", &child, e))?;
            if kind.is_symlink() {
                let text =
                    fs::read_link(&child).map_err(|e| io_error("cannot read link", &child, e))?;
                match fs::metadata(&child) {
                    Ok(metadata) if text.is_absolute() => kind = metadata.file_type(),
                    _ => {
                        let text = text.to_str().ok_or_else(|| {
                            format!("non-UTF-8 symlink target of '{}'", child.display())
                        })?;
                        tree.symlinks.push((child_relative, text.to_owned()));
                        continue;
                    }
                }
            }
            if kind.is_dir() {
                tree.directories.push(child_relative.clone());
                if !visit(&child, &child_relative, tree, depth + 1)? {
                    return Ok(false);
                }
            } else if kind.is_file() {
                let bytes =
                    fs::read(&child).map_err(|e| io_error("cannot read output", &child, e))?;
                tree.files.push((child_relative, bytes, true));
            } else {
                return Ok(false);
            }
        }
        Ok(true)
    }
    let mut tree = crate::remote_cache::DownloadedTree::default();
    Ok(visit(root, "", &mut tree, 0)?.then_some(tree))
}

/// Materializes one downloaded output at `path` in the execution root
/// (RemoteExecutionService.downloadOutputs and createSymlinks): a file
/// (executable like every remote output), a symlink with its text, or a
/// tree artifact's directories, files (with their executable bits) and
/// symlinks below the already created tree root. A missing output stays
/// missing (the publication reports it).
pub fn write_downloaded(
    path: &Path,
    output: &crate::remote_cache::Downloaded,
) -> Result<(), String> {
    use crate::remote_cache::Downloaded;
    match output {
        Downloaded::File(bytes) => write_executable_file(path, bytes, 0o111),
        Downloaded::Symlink(target) => std::os::unix::fs::symlink(target, path)
            .map_err(|e| io_error("cannot create symbolic link", path, e)),
        Downloaded::Directory(tree) => {
            ensure_directory(path)?;
            for directory in &tree.directories {
                validate_relative(Path::new(directory))?;
                ensure_directory(&path.join(directory))?;
            }
            for (name, bytes, executable) in &tree.files {
                validate_relative(Path::new(name))?;
                let file = path.join(name);
                if let Some(parent) = file.parent() {
                    ensure_directory(parent)?;
                }
                write_executable_file(&file, bytes, if *executable { 0o111 } else { 0 })?;
            }
            for (name, target) in &tree.symlinks {
                validate_relative(Path::new(name))?;
                let link = path.join(name);
                if let Some(parent) = link.parent() {
                    ensure_directory(parent)?;
                }
                std::os::unix::fs::symlink(target, &link)
                    .map_err(|e| io_error("cannot create symbolic link", &link, e))?;
            }
            Ok(())
        }
        // Missing stays missing (the publication reports it); a
        // remote-only file is published as metadata.
        Downloaded::Missing | Downloaded::Remote(_) => Ok(()),
    }
}

fn replay_logs(id: &str, stdout: &str, stderr: &str) {
    if !stdout.is_empty() || !stderr.is_empty() {
        // Bazel's `INFO: From …` event, the output bound to it: dropped
        // with it under --quiet (crate::ui).
        crate::note!("action '{id}' logs:\nstdout:\n{stdout}\nstderr:\n{stderr}");
    }
}

/// The outputs of a spawn that ran remotely or hit the remote cache.
struct RemoteDone {
    outputs: Vec<crate::remote_cache::Downloaded>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// A cache hit (GetActionResult, or an ExecuteResponse with
    /// `cached_result`): counted as a remote cache hit.
    cached: bool,
    /// The hit came from the `--disk_cache` (runner `disk cache hit`).
    disk: bool,
}

/// How a remote spawn ended.
enum RemoteRun {
    Done(RemoteDone),
    /// `--remote_local_fallback`: run it with the local strategy (and
    /// upload the result like RemoteSpawnRunner.execLocallyAndUpload).
    Fallback,
}

/// How [`remote_spawn`] ended.
pub enum RemoteSpawn {
    /// The downloaded result (any exit code) and whether it was a cache hit.
    Done {
        result: crate::remote_cache::DownloadedResult,
        cached: bool,
    },
    /// `--remote_local_fallback` after a failure.
    Fallback,
    /// An execution timeout (ExecutionStatusException.isExecutionTimeout:
    /// RemoteSpawnRunner.handleError's TIMEOUT result).
    Timeout,
    /// RemoteSpawnRunner.handleError's failure: `(Exit 34): MESSAGE`
    /// (ExitCode.REMOTE_ERROR, Utils.grpcAwareErrorMessage).
    Failed(String),
}

/// RemoteSpawnRunner.maybeGetMissingMandatoryOutput: a declared output the
/// result lacks.
fn missing_mandatory(
    result: &crate::reapi::ActionResult,
    declared: &[(String, crate::remote_cache::OutputKind)],
) -> bool {
    declared.iter().any(|(path, _)| {
        !result.output_files.iter().any(|file| &file.path == path)
            && !result.output_symlinks.iter().any(|link| &link.path == path)
            && !result
                .output_directories
                .iter()
                .any(|directory| &directory.path == path)
    })
}

/// RemoteSpawnRunner.exec (remote/RemoteSpawnRunner.java:175-380) for one
/// spawn: the remote cache lookup (with `--remote_accept_cached` for a
/// remotely `cacheable` spawn), then, retried by the ExecuteRetrier
/// (RetryInfo details, FAILED_PRECONDITION with MISSING violations, lost
/// output blobs), the input upload (forced after the first attempt),
/// Execute (with `skip_cache_lookup` once a cached result was unusable) and
/// the output download. `mandatory`: every declared output must be in a
/// usable cached result (a build action's; a test spawn declares none
/// mandatory). A failure falls back to local execution with
/// `--remote_local_fallback` (never for an execution timeout), else is
/// [`RemoteSpawn::Failed`]; `Err` is a credential helper or capability
/// failure, which never falls back.
pub fn remote_spawn(
    remote: &crate::remote_cache::RemoteCache,
    spec: &crate::remote_cache::RemoteAction,
    blobs: &BTreeMap<crate::reapi::Digest, crate::remote_cache::BlobSource<'_>>,
    declared: &[(String, crate::remote_cache::OutputKind)],
    cacheable: bool,
    mandatory: bool,
    stay_remote: &dyn Fn(usize) -> bool,
) -> Result<RemoteSpawn, String> {
    use crate::remote_cache::CacheError;
    use crate::remote_execution::ExecError;
    let executor = remote
        .executor()
        .ok_or("remote execution without an executor")?;
    let key = &spec.key;
    let missing =
        |result: &crate::reapi::ActionResult| mandatory && missing_mandatory(result, declared);
    // The input root's Directory blobs, the Action and the Command
    // (uploadInputsIfNotPresent's additionalInputs) join the input files.
    let mut blobs = blobs.clone();
    for (digest, directory) in &spec.directories {
        blobs
            .entry(digest.clone())
            .or_insert(crate::remote_cache::BlobSource::Bytes(directory));
    }
    blobs
        .entry(spec.key.clone())
        .or_insert(crate::remote_cache::BlobSource::Bytes(&spec.action));
    blobs
        .entry(spec.command_digest.clone())
        .or_insert(crate::remote_cache::BlobSource::Bytes(&spec.command));
    let blobs = &blobs;
    // execLocallyAndUploadOrFail.
    let fail = |error: ExecError| -> Result<RemoteSpawn, String> {
        if remote.local_fallback && !error.is_execution_timeout() {
            return Ok(RemoteSpawn::Fallback);
        }
        if error.is_execution_timeout() {
            if let ExecError::Execution {
                response: Some(response),
                ..
            } = &error
                && remote.print_execution_messages.should_print(false)
                && !response.message.is_empty()
            {
                crate::note!("Remote server execution message: {}", response.message);
            }
            remote.count_execution();
            return Ok(RemoteSpawn::Timeout);
        }
        crate::remote_cache::note_remote_failure();
        Ok(RemoteSpawn::Failed(format!(
            "(Exit 34): {}",
            error.message()
        )))
    };
    let cache_error = |error: CacheError| -> Result<Result<RemoteSpawn, String>, String> {
        match error {
            // Utils.createExecExceptionForCredentialHelperException.
            CacheError::Credentials(error) => Err(format!(
                "Exec failed due to CredentialHelperException: {error}"
            )),
            CacheError::Capabilities(error) => Err(error),
            CacheError::Other(message) => Ok(fail(ExecError::Io(message))),
        }
    };
    let mut accept_cached = remote.accept_cached && cacheable;
    if accept_cached {
        match remote.cached_result(key) {
            Ok(Some(result)) if result.exit_code == 0 && !missing(&result) => {
                match remote.download(&result, declared, key, stay_remote, false) {
                    Ok(Some(downloaded)) => {
                        remote.count_hit();
                        return Ok(RemoteSpawn::Done {
                            result: downloaded,
                            cached: true,
                        });
                    }
                    // A lost output blob: execute instead.
                    Ok(None) => accept_cached = false,
                    Err(error) => return cache_error(error)?,
                }
            }
            // Failed actions are cache misses (flaky tests).
            Ok(Some(_)) => accept_cached = false,
            Ok(None) => {}
            Err(error) => return cache_error(error)?,
        }
    }
    // One attempt's failure: retried after the delay (ExecuteRetrier's
    // transient classification), or final.
    enum Failed {
        Retry(ExecError, std::time::Duration),
        Final(ExecError),
        Credentials(String),
    }
    let mut use_cached = accept_cached;
    let mut force = false;
    let mut retries = 0u32;
    let max_retries = executor.retries();
    loop {
        let attempt = (|| -> Result<(crate::remote_cache::DownloadedResult, bool), Failed> {
            let upload_force = std::mem::replace(&mut force, true);
            remote
                .ensure_inputs_present(blobs, key, upload_force)
                .map_err(|error| match error {
                    CacheError::Credentials(error) => Failed::Credentials(format!(
                        "Exec failed due to CredentialHelperException: {error}"
                    )),
                    error => Failed::Final(ExecError::Io(error.to_string())),
                })?;
            let request = crate::remote_execution::execute_request(
                executor.instance_name(),
                key,
                !use_cached,
            );
            let response = crate::remote_execution::execute(executor, &request, &key.hash)
                .map_err(|error| match error.retry_delay() {
                    Some(delay) => Failed::Retry(error, delay),
                    None => Failed::Final(error),
                })?;
            let result =
                crate::reapi::ActionResult::decode(response.result.as_deref().unwrap_or(&[]))
                    .map_err(|error| Failed::Final(ExecError::Io(error)))?;
            if response.cached_result && (result.exit_code != 0 || missing(&result)) {
                // Retried with forced execution (a NOT_FOUND with RetryInfo).
                use_cached = false;
                return Err(Failed::Retry(
                    ExecError::Io("the cached result of the remote executor is unusable".into()),
                    std::time::Duration::ZERO,
                ));
            }
            if remote
                .print_execution_messages
                .should_print(result.exit_code == 0)
                && !response.message.is_empty()
            {
                crate::note!("Remote server execution message: {}", response.message);
            }
            match remote.download(&result, declared, key, stay_remote, !response.cached_result) {
                Ok(Some(downloaded)) => Ok((downloaded, response.cached_result)),
                // A BulkTransferException of CacheNotFoundExceptions:
                // retried without cached results.
                Ok(None) => {
                    use_cached = false;
                    Err(Failed::Retry(
                        ExecError::Io(format!(
                            "Failed to fetch the outputs of {} because they do not exist remotely.",
                            key.hash
                        )),
                        std::time::Duration::ZERO,
                    ))
                }
                Err(CacheError::Credentials(error)) => Err(Failed::Credentials(format!(
                    "Exec failed due to CredentialHelperException: {error}"
                ))),
                Err(error) => Err(Failed::Final(ExecError::Io(error.to_string()))),
            }
        })();
        match attempt {
            Ok((result, cached)) => {
                if cached {
                    remote.count_hit();
                } else {
                    remote.count_execution();
                }
                return Ok(RemoteSpawn::Done { result, cached });
            }
            Err(Failed::Credentials(message)) => return Err(message),
            Err(Failed::Retry(_, delay)) if retries < max_retries => {
                retries += 1;
                std::thread::sleep(delay);
            }
            Err(Failed::Retry(error, _) | Failed::Final(error)) => return fail(error),
        }
    }
}

/// [`remote_spawn`] for a build action: a non-zero exit fails it like a
/// local spawn's, an execution timeout with `(Exit 142)`.
fn run_remotely(
    remote: &crate::remote_cache::RemoteCache,
    action: &Action,
    spec: &crate::remote_cache::RemoteAction,
    blobs: &BTreeMap<crate::reapi::Digest, crate::remote_cache::BlobSource<'_>>,
) -> Result<RemoteRun, String> {
    let declared = declared_outputs(action)?;
    let failure = format!("action '{}'", action.id);
    let stay_remote = |index: usize| {
        crate::remote_outputs::may_stay_remote(&action.id, index, &action.outputs[index])
    };
    match remote_spawn(
        remote,
        spec,
        blobs,
        &declared,
        remote_policy(action).0,
        true,
        &stay_remote,
    )? {
        RemoteSpawn::Fallback => Ok(RemoteRun::Fallback),
        RemoteSpawn::Failed(message) => Err(format!("{failure} failed: {message}")),
        RemoteSpawn::Timeout => Err(format!(
            "{failure} failed: (Exit 142): remote spawn timed out (failed due to timeout.)"
        )),
        RemoteSpawn::Done { result, .. } if result.exit_code != 0 => Err(format!(
            "{failure} failed (exit status: {}):\nstdout:\n{}\nstderr:\n{}",
            result.exit_code,
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        )),
        RemoteSpawn::Done { result, cached } => Ok(RemoteRun::Done(RemoteDone {
            disk: false,
            outputs: result.outputs,
            stdout: result.stdout,
            stderr: result.stderr,
            cached,
        })),
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
    // Java protocol capability selection precedes terminal cache lookup too:
    // an explicitly requested unsupported backend must not appear supported
    // merely because a previous local invocation populated the cache.
    let java_strategy = if action.extra.java_compile.is_some() {
        let mut info = action.execution_info.clone();
        info.extend(action.extra.execution_info.keys().cloned());
        let strategy = crate::sandbox::resolve(&action.extra.mnemonic, &info, false)?;
        if strategy == crate::sandbox::Strategy::Remote {
            return Err("JavaBuilder reduced/fallback remote execution is not implemented in the native profile".into());
        }
        if remote.is_some() {
            crate::note!(
                "JavaBuilder protocol spawn-cache lookup/upload is unsupported; using the native terminal action cache"
            );
        }
        Some(strategy)
    } else {
        None
    };
    let remote = if java_strategy.is_some() {
        None
    } else {
        remote
    };
    if remote.is_some() {
        note_download_store(cache_root);
    }
    let embedded;
    let action = if action.embedded_inputs.is_empty() {
        action
    } else {
        // Compiled-in inputs live in the store: validate first, then create
        // the cache to store them.
        validate_structure(action)?;
        let (_, cas) = cache_store(cache_root)?;
        embedded = with_embedded_inputs(action, &cas)?;
        &embedded
    };
    let mut clock = std::time::Instant::now();
    let existing = ensured_cache(cache_root)
        .map(|directories| directories.root.clone())
        .or_else(|| fs::canonicalize(cache_root).ok());
    count_check(0, &mut clock);
    let prepared = prepare(action, existing.as_deref())?;
    count_check(1, &mut clock);
    let (inputs, outputs, key) = (&prepared.inputs, &prepared.outputs, &prepared.key);
    let directories = ensure_cache(cache_root)?;
    let CacheDirectories {
        root,
        actions,
        locks,
        work: work_parent,
        cas,
    } = &*directories;
    let (root, cas): (&Path, &Path) = (root, cas);
    count_check(0, &mut clock);
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
            remote_executed: false,
            runner: None,
            additional_spawns: 0,
            critical: std::time::Duration::ZERO,
            trees: action
                .outputs
                .iter()
                .enumerate()
                .filter_map(|(index, name)| Some((index, listings.get(name)?.clone())))
                .collect(),
        }
    };
    // A valid entry needs no lock: entries are published by an atomic
    // rename and only ever removed, so a check that finds every output
    // valid found one complete entry (a check racing a removal fails and
    // takes the lock below). The lock serializes checking and executing one
    // key across processes; it never protected an entry after its check.
    // Stamps to record (crate::action::tree_stamps) are written under it.
    // Build without the Bytes: which outputs may stay remote-only
    // (crate::remote_outputs, by declaration index).
    let stay_remote = |name: &Path| -> bool {
        action
            .outputs
            .iter()
            .position(|output| output == name)
            .is_some_and(|index| crate::remote_outputs::may_stay_remote(&action.id, index, name))
    };
    if let Some((listings, false)) = cache_valid(
        &cached,
        key,
        outputs,
        (&action.extra.tree_outputs, &action.extra.symlink_outputs),
        root,
        false,
        &stay_remote,
    ) {
        count_check(3, &mut clock);
        CHECKS[4].fetch_add(1, Ordering::Relaxed);
        return Ok(result(listings, true, false));
    }
    count_check(3, &mut clock);
    let lock_path = locks.join(format!("{key}.lock"));
    let lock = match open_lock(&lock_path) {
        // The cache was removed since this process ensured its directories
        // (only tests do that): create them again.
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            forget_ensured_cache(cache_root);
            ensure_cache(cache_root)?;
            open_lock(&lock_path)
        }
        opened => opened,
    };
    // Locks are never unlinked: removing a held lock would permit two distinct
    // inodes (and thus two owners) for the same action key.
    let lock = match lock {
        Err(error) if not_a_lock_file(&error) => None,
        opened => Some(opened.map_err(|e| io_error("cannot open lock", &lock_path, e))?),
    }
    .filter(|lock| lock.metadata().is_ok_and(|metadata| metadata.is_file()));
    let Some(lock) = lock else {
        return Err(format!(
            "expected regular lock file: '{}'",
            lock_path.display()
        ));
    };
    FileExt::lock_exclusive(&lock).map_err(|e| io_error("cannot lock", &lock_path, e))?;
    count_check(2, &mut clock);
    // File drop releases the advisory lock on every success/error path.
    if let Some((listings, _)) = cache_valid(
        &cached,
        key,
        outputs,
        (&action.extra.tree_outputs, &action.extra.symlink_outputs),
        root,
        true,
        &stay_remote,
    ) {
        count_check(3, &mut clock);
        CHECKS[4].fetch_add(1, Ordering::Relaxed);
        return Ok(result(listings, true, false));
    }
    count_check(3, &mut clock);
    CHECKS[5].fetch_add(1, Ordering::Relaxed);
    // This entry's trees and files are no longer what the memos hold: they
    // are removed now and published again below.
    forget_trees(&cached);
    forget_outputs(&cached);
    forget_published(&cached);
    crate::remote_outputs::forget_entry(&cached);
    remove_stale(&cached)?;
    let work = fresh_work(work_parent, key)?;
    let execroot = work.0.join("execroot");
    let tmp = local_tmpdir();
    ensure_directory(&execroot)?;
    let process = action.write.is_none() && action.extra.transform.is_none();
    let module = remote;
    // The execution info merges the rule tags with the spawn's
    // execution_requirements map, exactly like remote_policy
    // (crate::action::remote_policy).
    let execution_info = {
        let mut merged = action.execution_info.clone();
        merged.extend(action.extra.execution_info.keys().cloned());
        merged
    };
    // With a remote executor the strategy is resolved first: the `remote`
    // strategy does its own cache lookup (RemoteSpawnRunner.handlesCaching)
    // and the remote cache serves the other strategies' spawns
    // (RemoteSpawnCache).
    let can_worker = process
        && java_strategy.is_none()
        && crate::worker::can_exec(
            &action.extra.mnemonic,
            &action.extra.execution_info,
            action.executable_file,
        );
    let early_strategy = match module {
        Some(module) if process && module.executor().is_some() => Some(
            crate::sandbox::resolve(&action.extra.mnemonic, &execution_info, can_worker)
                .map_err(|error| format!("action '{}': {error}", action.id))?,
        ),
        _ => None,
    };
    let mut remote_exec = early_strategy == Some(crate::sandbox::Strategy::Remote);
    // The remote cache applies to cacheable process actions only
    // (RemoteSpawnCache.java:85-92).
    // The read and write policies of the spawn cache (the remote cache for
    // remotely cacheable spawns, the `--disk_cache` for cacheable ones).
    let (read, write) = match module {
        Some(module) if process => {
            let (remotely, _) = remote_policy(action);
            let cacheable = may_be_cached(action);
            (
                module.read_policy(cacheable, remotely),
                module.write_policy(cacheable, remotely),
            )
        }
        _ => Default::default(),
    };
    let remote = if remote_exec {
        module
    } else {
        module.filter(|_| process && (read.any() || write.any()))
    };
    // Tree artifact outputs use ActionResult.output_directories and their
    // `Tree` (UploadManifest.addDirectory; downloaded like
    // RemoteExecutionService.downloadOutputs).
    let unsupported = action.remote_unsupported.as_deref();
    let remote = match (remote, unsupported) {
        (Some(_), Some(reason)) => {
            crate::remote_cache::warn(&crate::remote_cache::CacheError::Other(format!(
                "not used for {reason}"
            )));
            None
        }
        (remote, _) => remote,
    };
    // The gRPC capability check precedes the key (initUseOutputPaths); its
    // failure fails the action (RemoteSpawnCache.java:189-197).
    let output_paths = match remote.map(|remote| remote.output_paths()) {
        Some(Err(crate::remote_cache::CacheError::Capabilities(error))) => return Err(error),
        Some(Err(error)) => return Err(error.to_string()),
        Some(Ok(output_paths)) => output_paths,
        None => true,
    };
    // PlatformUtils.getPlatformProto: the spawn's Platform, with
    // `--remote_default_exec_properties` (a parse failure fails the spawn).
    let platform = match remote {
        Some(remote) => crate::remote_execution::platform_properties(
            &action.remote_platform,
            &remote.default_exec_properties,
        )
        .map_err(|error| format!("action '{}': {error}", action.id))?,
        None => Vec::new(),
    };
    let remote_spec = match remote {
        Some(_) => match remote_action(action, &prepared, output_paths, &platform) {
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
    remote_exec &= remote.is_some();
    let mut remote_hit: Option<RemoteDone> = None;
    // `--remote_local_fallback` after a failed remote execution.
    let mut fallback = false;
    if remote_exec && let (Some(remote), Some((spec, blobs))) = (remote, &remote_spec) {
        match run_remotely(remote, action, spec, blobs)? {
            RemoteRun::Done(done) => remote_hit = Some(done),
            RemoteRun::Fallback => fallback = true,
        }
    } else if let (Some(remote), Some((spec, _))) = (remote, &remote_spec)
        && read.any()
    {
        let declared = declared_outputs(action)?;
        let stay_remote = |index: usize| {
            crate::remote_outputs::may_stay_remote(&action.id, index, &action.outputs[index])
        };
        match remote.lookup(spec, &declared, &stay_remote, read, write) {
            Ok(hit) => {
                remote_hit = hit.map(|hit| RemoteDone {
                    disk: hit.source == crate::remote_cache::CacheSource::Disk,
                    outputs: hit
                        .outputs
                        .into_iter()
                        .map(|output| match output {
                            crate::remote_cache::Output::File(bytes) => {
                                crate::remote_cache::Downloaded::File(bytes)
                            }
                            crate::remote_cache::Output::Symlink(target) => {
                                crate::remote_cache::Downloaded::Symlink(target)
                            }
                            crate::remote_cache::Output::Remote(digest) => {
                                crate::remote_cache::Downloaded::Remote(digest)
                            }
                            crate::remote_cache::Output::Directory(tree) => {
                                crate::remote_cache::Downloaded::Directory(tree)
                            }
                        })
                        .collect(),
                    stdout: hit.stdout,
                    stderr: hit.stderr,
                    cached: true,
                })
            }
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
    // A local fallback uploads like RemoteSpawnRunner.execLocallyAndUpload
    // (the write policy: a remotely cacheable spawn).
    let remote = remote.filter(|_| !remote_exec || (fallback && remote_policy(action).0));
    let remote_spec = remote_spec.map(|(spec, _)| spec);
    // The spawn strategy (N59): the highest-priority strategy that can
    // execute this spawn (SpawnStrategyResolver.resolveOne), resolved per
    // action from its mnemonic before anything is staged — a worker spawn
    // (crate::worker) stages into its worker's working directory. The
    // strategy never changes the action key: a cache entry produced
    // sandboxed, by a worker or standalone satisfies any request, like
    // Bazel's action cache. The execution info merges the rule tags with
    // the spawn's execution_requirements map, exactly like remote_policy
    // (crate::action::remote_policy).
    let mut spawn_strategy = None;
    let mut worker: Option<(crate::worker::Lease, Vec<String>)> = None;
    if process && remote_hit.is_none() {
        // A failed remote spawn falls back to
        // `--remote_local_fallback_strategy`, `local` (standalone).
        let strategy = match early_strategy {
            _ if fallback => crate::sandbox::Strategy::Standalone,
            Some(strategy) => strategy,
            None if java_strategy.is_some() => java_strategy.expect("checked Java strategy"),
            None => crate::sandbox::resolve(&action.extra.mnemonic, &execution_info, can_worker)
                .map_err(|error| format!("action '{}': {error}", action.id))?,
        };
        if strategy == crate::sandbox::Strategy::Worker {
            // WorkerParser.compute: the worker key's environment is the
            // spawn's with the local TMPDIR (PosixLocalEnvProvider).
            let mut environment = resolved_environment(action);
            environment.insert("TMPDIR".into(), tmp.clone().into_os_string());
            let config = crate::worker::parse(
                &action.extra.mnemonic,
                &arguments(action),
                environment,
                &action.extra.execution_info,
            )
            .map_err(|error| {
                if error.environmental {
                    // Genuine's IOException class: exit code 36.
                    crate::sandbox::note_setup_failure();
                }
                format!("action '{}' failed: {}", action.id, error.message)
            })?;
            let lease = crate::worker::acquire(work_parent, config.key)
                .map_err(|error| format!("action '{}' failed: {error}", action.id))?;
            // The previous request's inputs and outputs go; the worker
            // process keeps its working directory (its cwd).
            clear_directory(lease.execroot())?;
            worker = Some((lease, config.flag_files));
        }
        spawn_strategy = Some((strategy, execution_info.clone()));
    }
    let execroot = match &worker {
        Some((lease, _)) => lease.execroot().to_path_buf(),
        None => execroot,
    };
    // Only a process reads its inputs through the execution root; native
    // writes and transforms read the verified input files directly, and a
    // remote hit runs nothing — unless a downloaded tree artifact holds
    // symlinks: their targets may be inputs (an OCI layout's blobs link
    // to its layers), which genuine's execution root always holds.
    let hit_links = remote_hit.as_ref().is_some_and(|hit| {
        hit.outputs.iter().any(|output| {
            matches!(output, crate::remote_cache::Downloaded::Directory(tree) if !tree.symlinks.is_empty())
        })
    });
    let staged = if process && (remote_hit.is_none() || hit_links) && java_strategy.is_none() {
        stage_inputs(cas, root, inputs, &execroot)?
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
    // The spawn's runner name (SpawnResult.getRunnerName); none for a write
    // or a native transform.
    let mut runner: Option<&'static str> = None;
    let mut additional_spawns = 0;
    // Whether the spawn ran in a sandboxed worker (`--worker_sandboxing`).
    let worker_sandboxed = worker.as_ref().is_some_and(|(lease, _)| lease.sandboxed());
    if let Some(write) = &action.write {
        write_executable_file(
            &execroot.join(&outputs[0]),
            &write.content,
            if write.executable { 0o111 } else { 0 },
        )?;
    } else if let Some(transform) = &action.extra.transform {
        // A native transform reads or links its inputs in place: remote-only
        // ones are materialized first (a runfiles tree links every input);
        // a runfiles manifest records paths only.
        if !matches!(transform, Transform::RunfilesManifest { .. }) {
            for (source, _) in &action.inputs {
                materialize(cas, root, source.as_path())?;
            }
        }
        run_transform(action, transform, &prepared, &execroot, (cas, root))?;
    } else if let Some(hit) = &remote_hit {
        runner = Some(if hit.cached && hit.disk {
            "disk cache hit"
        } else if hit.cached && !remote_exec {
            "remote cache hit"
        } else {
            "remote"
        });
        crate::progress::spawn_started(if hit.cached {
            crate::progress::Strategy::RemoteCache
        } else {
            crate::progress::Strategy::Remote
        });
        // The downloaded (verified) outputs take the place of the process's;
        // they are published below like executed outputs.
        for (name, output) in action.outputs.iter().zip(&hit.outputs) {
            write_downloaded(&execroot.join(name), output)?;
        }
        replay_logs(
            &action.id,
            &String::from_utf8_lossy(&hit.stdout),
            &String::from_utf8_lossy(&hit.stderr),
        );
        if crate::exec_log::active() {
            // SpawnResult runner names: RemoteSpawnCache's "remote cache
            // hit", RemoteSpawnRunner's "remote" (a server-side cached
            // result is a cache hit of the remote runner).
            log_spawn(
                action,
                &prepared,
                &execroot,
                if hit.cached && hit.disk {
                    "disk cache hit"
                } else if hit.cached && !remote_exec {
                    "remote cache hit"
                } else {
                    "remote"
                },
                0,
                hit.cached,
                (started, clock.elapsed()),
                remote_spec.as_ref().map(|spec| &spec.key),
            );
        }
    } else if let Some(strategy) = java_strategy {
        let outcome = java::run(
            action, &prepared, &work.0, &execroot, &tmp, cas, root, strategy,
        );
        match outcome {
            Ok(fallback) => {
                sandboxed = strategy == crate::sandbox::Strategy::Sandboxed;
                runner = Some(strategy.runner_name());
                additional_spawns = u8::from(fallback);
            }
            Err(error) => {
                if strategy == crate::sandbox::Strategy::Sandboxed && crate::sandbox::debug() {
                    let path = work.0.display().to_string();
                    std::mem::forget(work);
                    return Err(format!(
                        "{error}\n\n(sandbox build root retained at '{path}')"
                    ));
                }
                return Err(error);
            }
        }
    } else if let Some((lease, flag_files)) = worker.as_mut() {
        // A persistent worker (crate::worker, WorkerSpawnRunner.exec): the
        // flag files expanded into a WorkRequest, the response's output is
        // the spawn's stderr (`response.getOutputBytes().writeTo(outErr
        // .getErrorStream())`), its exit code the spawn's.
        report_subcommand(action, &execroot, "worker");
        crate::progress::spawn_started(crate::progress::Strategy::Worker);
        runner = Some("worker");
        let request_inputs = worker_request_inputs(inputs, &action.extra.param_files);
        let read = |path: &str| -> Option<io::Result<Vec<u8>>> {
            declared_input(inputs, &action.extra.param_files, Path::new(path))
                .then(|| fs::read(execroot.join(path)))
        };
        let response = lease.execute(flag_files, &read, request_inputs);
        crate::sandbox::count(crate::sandbox::Strategy::Worker);
        let response =
            response.map_err(|error| format!("action '{}' failed: {error}", action.id))?;
        check_staged(&staged, &execroot).map_err(|e| format!("action '{}': {e}", action.id))?;
        if crate::exec_log::active() {
            log_spawn(
                action,
                &prepared,
                &execroot,
                "worker",
                response.exit_code,
                false,
                (started, clock.elapsed()),
                remote_spec.as_ref().map(|spec| &spec.key),
            );
        }
        if response.exit_code != 0 {
            return Err(format!(
                "action '{}' failed (exit status: {}):\nstdout:\n\nstderr:\n{}",
                action.id, response.exit_code, response.output
            ));
        }
        replay_logs(&action.id, "", &response.output);
        if remote_spec.is_some() {
            logs = (Vec::new(), response.output.into_bytes());
        }
    } else {
        let mut stdout = create_log(&work.0.join("stdout.log"))?;
        let mut stderr = create_log(&work.0.join("stderr.log"))?;
        let argv = arguments(action);
        let (strategy, execution_info) = spawn_strategy
            .take()
            .expect("a process spawn resolved its strategy");
        sandboxed = strategy == crate::sandbox::Strategy::Sandboxed;
        runner = Some(strategy.runner_name());
        report_subcommand(action, &execroot, strategy.runner_name());
        crate::progress::spawn_started(match strategy {
            crate::sandbox::Strategy::Sandboxed => crate::progress::Strategy::LinuxSandbox,
            crate::sandbox::Strategy::ProcessWrapper => {
                crate::progress::Strategy::ProcessWrapperSandbox
            }
            _ => crate::progress::Strategy::Local,
        });
        let environment = resolved_environment(action);
        // Every spawn runs under a supervisor that outlives it (Bazel's
        // process-wrapper or linux-sandbox; crate::process_wrapper): a child
        // subreaper that, once the action has exited, kills and reaps every
        // process the action left — also those that left its process group
        // (setsid daemons, double forks) — before the outputs are checked,
        // hashed and published, so no process of the action can change a
        // staged input after `check_staged` or an output after it was hashed
        // (published digests are trusted for the rest of the command and,
        // through the persistent digest memo, by later commands). The action
        // leads its own process group, which the memory watchdog kills on a
        // ceiling hit (crate::memory_limit).
        let mut launch = if sandboxed {
            // The executor starts its own binary (/proc/self/exe) as the
            // sandbox worker (the role linux-sandbox fills as the spawn's
            // argv[0], SandboxedSpawnStrategy.exec ->
            // LinuxSandboxCommandLineBuilder.buildForCommand) with the
            // action's cwd and environment; the worker applies the sandbox
            // and supervises the action. The wrapped argv carries the
            // action's own arguments after --.
            crate::process_wrapper::Launch::sandbox_worker(crate::sandbox::wrap(
                &execroot,
                &tmp,
                &environment,
                crate::sandbox::denies_network(&execution_info),
                &argv,
                &work.0,
                None,
            ))
        } else if action.executable_file && !argv[0].starts_with('/') {
            // Bazel runs a File executable by its execution path, relative
            // to the execution root; argv[0] keeps that spelling.
            crate::process_wrapper::Launch::new(
                execroot.join(&argv[0]),
                Some(OsStr::new(&argv[0])),
                &argv[1..],
            )
        } else {
            crate::process_wrapper::Launch::new(&argv[0], None, &argv[1..])
        };
        let command = launch.command();
        command.current_dir(&execroot).env_clear();
        // Exactly resolved_environment (ActionEnvironment.resolve,
        // ActionEnvironment.java:113-122), the remote key's environment;
        // then PosixLocalEnvProvider's TMPDIR (local_tmpdir), last.
        command.envs(&environment);
        command
            .env("TMPDIR", &tmp)
            .stdout(Stdio::from(stdout.try_clone().map_err(|e| e.to_string())?))
            .stderr(Stdio::from(stderr.try_clone().map_err(|e| e.to_string())?));
        let mut child = retry_spawn(|| launch.start(), std::thread::sleep)
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
        if crate::exec_log::active() {
            log_spawn(
                action,
                &prepared,
                &execroot,
                match strategy {
                    crate::sandbox::Strategy::Sandboxed => "linux-sandbox",
                    crate::sandbox::Strategy::ProcessWrapper => "processwrapper-sandbox",
                    _ => "local",
                },
                status.code().unwrap_or(1),
                false,
                (started, clock.elapsed()),
                remote_spec.as_ref().map(|spec| &spec.key),
            );
        }
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
    // Tree outputs are read (and the blobs of their linked files ensured)
    // before anything is published, while every path their symlinks can
    // name is in place: the inputs, and the other outputs, which publication
    // moves. Every output is normalized first (a regular one here, a tree's
    // entries by their walk), so that a link to another output of the action
    // shows its final mode, as it does once Bazel collected the outputs.
    let mut tree_reads: BTreeMap<&PathBuf, Result<ReadTree, String>> = BTreeMap::new();
    if symlink_target.is_none() && !action.extra.tree_outputs.is_empty() {
        for name in outputs
            .iter()
            .filter(|name| !action.extra.tree_outputs.contains(*name))
        {
            // publish_file checks and normalizes it again.
            let path = execroot.join(name);
            if regular_file(&path, None).is_ok() {
                let _ = set_mode(&path, OUTPUT_MODE);
            }
        }
        let persistent_execroot = (!sandboxed && !worker_sandboxed).then_some(execroot.as_path());
        let walks: Vec<(&PathBuf, Result<WalkedTree, String>)> = outputs
            .iter()
            .filter(|name| action.extra.tree_outputs.contains(*name))
            .map(|name| {
                let pretty = pretty_output(name);
                let output = OutputTree {
                    pretty: &pretty,
                    work: &work.0,
                    execroot: persistent_execroot,
                };
                (
                    name,
                    walk_tree(&execroot.join(name), Some(root), TreeRead::Output(output)),
                )
            })
            .collect();
        let reads: Vec<_> = walks
            .into_iter()
            .map(|(name, walked)| (name, walked.and_then(read_walked)))
            .collect();
        for (name, read) in reads {
            let read = read.and_then(|(tree, states)| read_output_tree(cas, root, tree, states));
            tree_reads.insert(name, read);
        }
    }
    // Remote-only outputs (Build without the Bytes): published as metadata.
    let remote_only: BTreeMap<&PathBuf, &crate::reapi::Digest> = match &remote_hit {
        Some(hit) => action
            .outputs
            .iter()
            .zip(&hit.outputs)
            .filter_map(|(name, output)| match output {
                crate::remote_cache::Downloaded::Remote(digest) => Some((name, digest)),
                _ => None,
            })
            .collect(),
        None => BTreeMap::new(),
    };
    let expires = if remote_only.is_empty() {
        0
    } else {
        crate::remote_outputs::expiration()
    };
    let mut registered = Vec::new();
    // The tree outputs as the spawn left them (UploadManifest reads the
    // execution root: a symlink stays one), read before publication
    // normalizes them, for the remote cache upload below.
    let mut tree_uploads: BTreeMap<&PathBuf, Option<crate::remote_cache::DownloadedTree>> =
        BTreeMap::new();
    if remote_hit.is_none()
        && remote_spec.is_some()
        && remote.is_some_and(|_| remote_write(remote_exec, fallback, write).any())
    {
        for name in &action.extra.tree_outputs {
            let tree = tree_upload(&execroot.join(name)).unwrap_or_else(|error| {
                crate::remote_cache::warn(&crate::remote_cache::CacheError::Other(error));
                None
            });
            tree_uploads.insert(name, tree);
        }
    }
    let mut listings = BTreeMap::new();
    // Published tree outputs, for the tree memo once the entry is in place.
    let mut published_trees = Vec::new();
    // Published regular files and their contents, for the digest memos.
    let mut published_files = Vec::new();
    for name in outputs {
        let source = execroot.join(name);
        let target = completed.join("outputs").join(name);
        let tree_output = action.extra.tree_outputs.contains(name);
        let mut publish = || -> Result<Output, String> {
            let hex_name = hex(name.as_os_str().as_bytes());
            if let Some(digest) = remote_only.get(name) {
                registered.push((name.clone(), (*digest).clone()));
                return Ok(Output {
                    name: hex_name,
                    digest: digest.hash.clone(),
                    mode: OUTPUT_MODE,
                    tree: None,
                    symlink: None,
                    runfiles: None,
                    remote: Some(RemoteRecord {
                        size: digest.size,
                        expires,
                    }),
                });
            }
            // An unresolved symlink output: the symlink itself, published
            // as is; its value is its link text
            // (ActionOutputMetadataStore.fileArtifactValueFromArtifact ->
            // FileArtifactValue.createForUnresolvedSymlink), and anything
            // else at its path fails like SkyframeActionExecutor
            // .checkOutputs (NotASymlinkException).
            if action.extra.symlink_outputs.contains(name) {
                let metadata = fs::symlink_metadata(&source)
                    .map_err(|e| io_error("cannot inspect", &source, e))?;
                if !metadata.file_type().is_symlink() {
                    return Err(format!(
                        "declared output '{}' is not a symlink",
                        pretty_output(name)
                    ));
                }
                let link =
                    fs::read_link(&source).map_err(|e| io_error("cannot read link", &source, e))?;
                if let Some(parent) = target.parent() {
                    ensure_directory(parent)?;
                }
                fs::rename(&source, &target).map_err(|e| io_error("cannot publish", &target, e))?;
                let text = link.as_os_str().as_bytes();
                return Ok(Output {
                    name: hex_name,
                    digest: unresolved_symlink_digest(text).hex(),
                    mode: metadata.permissions().mode() & 0o7777,
                    tree: None,
                    symlink: Some(hex(text)),
                    runfiles: None,
                    remote: None,
                });
            }
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
                            Staged::Tree(_) | Staged::SourceDirectory(_) | Staged::Symlink(_) => {
                                unreachable!("checked by run_transform")
                            }
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
                        remote: None,
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
                            remote: None,
                        }
                    }
                    Staged::SourceDirectory(_) | Staged::Symlink(_) => {
                        unreachable!("checked by run_transform")
                    }
                });
            }
            if tree_output {
                let read = tree_reads
                    .remove(name)
                    .expect("every tree output was read")?;
                let tree = publish_tree(cas, root, &source, &target, read)?;
                listings.insert(name.clone(), tree_listing(&tree));
                let output = Output {
                    name: hex_name,
                    digest: String::new(),
                    mode: TREE_ROOT_MODE,
                    tree: Some(tree_manifest(&tree)),
                    symlink: None,
                    runfiles: None,
                    remote: None,
                };
                published_trees.push((name, tree));
                return Ok(output);
            }
            // Match metadata collection order: a successful action may leave a
            // regular output unreadable. Normalize before reading its bytes,
            // but never chmod a symlink or a path through a linked ancestor.
            let input = publish_file(cas, root, &source, &target)?;
            let digest = input.digest;
            published_files.push((name, input));
            Ok(Output {
                name: hex_name,
                digest: digest.hex(),
                mode: OUTPUT_MODE,
                tree: None,
                symlink: None,
                runfiles: None,
                remote: None,
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
    for (name, digest) in registered {
        crate::remote_outputs::register(
            cached.join("outputs").join(&name),
            name,
            crate::remote_outputs::RemoteFile {
                hash: digest.hash,
                size: digest.size,
            },
        );
    }
    // What a consumer would read from the published entry: the same files,
    // digests and (normalized) modes at their published paths.
    for (name, input) in &published_files {
        remember_published(cas, &cached, &cached.join("outputs").join(name), input);
    }
    for (name, tree) in published_trees {
        let path = cached.join("outputs").join(name);
        if published_output(Some(root), &path) {
            let tree = relocated(tree, &path);
            for input in tree.files.values() {
                remember_published(cas, &cached, &input.path, input);
            }
            remember_tree(path, Arc::new(tree));
        }
    }
    if let Some(hit) = &remote_hit {
        // A remotely executed spawn counts as executed; the remote executor
        // stored its result.
        let mut done = result(listings, false, hit.cached);
        done.remote_executed = !hit.cached;
        done.runner = runner;
        return Ok(done);
    }
    if let (Some(remote), Some(spec)) = (remote, &remote_spec)
        && remote_write(remote_exec, fallback, write).any()
    {
        let uploads = action
            .outputs
            .iter()
            .map(|name| {
                let path = cached.join("outputs").join(name);
                let output = if action.extra.symlink_outputs.contains(name) {
                    symlink_upload(&path)?
                } else if let Some(tree) = tree_uploads.remove(name) {
                    tree.map(crate::remote_cache::Output::Directory)
                } else {
                    Some(crate::remote_cache::Output::File(
                        fs::read(&path).map_err(|e| io_error("cannot read output", &path, e))?,
                    ))
                };
                Ok(output.map(|output| (crate::remote_cache::path_string(name), output)))
            })
            .collect::<Result<Option<Vec<_>>, String>>()?;
        // RemoteExecutionService.reportUploadError: every upload failure,
        // credential helper failures included, is a warning.
        match uploads {
            Some(uploads) => {
                let uploads = uploads
                    .into_iter()
                    .map(|(name, output)| Ok((name?, output)))
                    .collect::<Result<Vec<_>, String>>()?;
                if let Err(error) = remote.upload(
                    spec,
                    &uploads,
                    &logs.0,
                    &logs.1,
                    started,
                    wall,
                    remote_write(remote_exec, fallback, write),
                ) {
                    crate::remote_cache::warn(&error);
                }
            }
            None => crate::remote_cache::warn(&crate::remote_cache::CacheError::Other(format!(
                "not uploaded for {}: an absolute symlink output to a directory or a special file in a tree artifact",
                action.id
            ))),
        }
    }
    // --sandbox_debug retains the sandbox build root after the build too
    // (SandboxOptions.sandbox_debug: "the sandbox root contents are left
    // untouched after a build").
    if sandboxed && crate::sandbox::debug() {
        std::mem::forget(work);
    }
    let mut done = result(listings, false, false);
    done.runner = runner;
    done.additional_spawns = additional_spawns;
    Ok(done)
}

mod java;
mod tree_stamps;

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::process::Command;
    use std::sync::{Arc, Barrier};

    include!("../tests/support/executable_script.rs");

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root = crate::test_temp::root();
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
                inputs: vec![(source.into(), PathBuf::from("src/input").into())],
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
                inputs: vec![(source.into(), "tools/quoted '$ tool".into())],
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
                (data.clone().into(), "pkg/data.txt".into()),
                (
                    manifest.clone().into(),
                    "bin/pkg/b.runfiles_manifest".into(),
                ),
                (exe.clone().into(), "bin/pkg/b".into()),
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
                    ]
                    .into(),
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
        // Settled, the tree's directories are stamped
        // (crate::action::tree_stamps): a later check lists nothing, and an
        // entry added to any of them still rebuilds the tree.
        crate::dir_stamps::SETTLE_ALL.with(|settle| settle.set(true));
        let entry = tree.ancestors().nth(4).unwrap().to_path_buf();
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        assert!(entry.join(tree_stamps::FILE_NAME).exists());
        let stamped = tree_stamps::COUNTS[0].load(Ordering::Relaxed);
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        assert!(tree_stamps::COUNTS[0].load(Ordering::Relaxed) > stamped);
        fs::write(tree.join("_main/pkg/extra"), b"").unwrap();
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        assert!(!tree.join("_main/pkg/extra").exists());
        crate::dir_stamps::SETTLE_ALL.with(|settle| settle.set(false));
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
    fn namespace_overlap_prefilter_finds_every_equal_or_containing_pair() {
        let candidates = [
            "", "a", "a/b", "a/b-x", "a/b/c", "a//b/c", "a/./b", "a/b/", "b", "/abs", "/abs/x",
            "a/../b", "a/..",
        ];
        // Every small namespace (inputs, outputs, parameter files) over the
        // candidates against the pairwise definition.
        for mask in 0u32..(1 << 9) {
            let pick = |offset: usize| -> Vec<PathBuf> {
                (0..3)
                    .filter(|bit| mask & (1 << (offset + bit)) != 0)
                    .map(|bit| {
                        PathBuf::from(candidates[(offset * 3 + bit * 5 + mask as usize) % 13])
                    })
                    .collect()
            };
            let inputs = pick(0);
            let outputs = pick(3);
            let params = pick(6);
            let mut action = Action {
                inputs: inputs
                    .iter()
                    .map(|p| (PathBuf::from("src").into(), p.into()))
                    .collect(),
                outputs: outputs.clone(),
                ..Action::default()
            };
            action.extra.param_files = params.iter().map(|p| (p.clone(), Vec::new())).collect();
            // The precondition (validate_structure checks every path first).
            if !inputs
                .iter()
                .chain(&outputs)
                .chain(&params)
                .all(|path| validate_relative(path).is_ok())
            {
                continue;
            }
            let mut distinct: Vec<&Path> = Vec::new();
            for input in &inputs {
                if !distinct.contains(&input.as_path()) {
                    distinct.push(input);
                }
            }
            distinct.extend(outputs.iter().chain(&params).map(PathBuf::as_path));
            let expected = distinct.iter().enumerate().any(|(i, a)| {
                distinct
                    .iter()
                    .enumerate()
                    .any(|(j, b)| i != j && (a.starts_with(b) || b.starts_with(a)))
            });
            assert_eq!(
                namespace_may_overlap(&action),
                expected,
                "{inputs:?} {outputs:?} {params:?}"
            );
        }
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
                    .push((fixture.0.join("missing").into(), "input".into())),
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
    fn shell_environment_is_exact_keyed_and_tmpdir_is_the_local_one() {
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
        // never reaches the process; the client's TMPDIR (or /tmp) does.
        action.command = format!(
            "test \"$PATH\" = /explicit/path && test \"$LANG\" = POSIX && test \"$LC_ALL\" = POSIX && test \"$TMPDIR\" = '{}' && test -d \"$TMPDIR\" && test \"${{EMPTY+x}}\" = x && test -z \"$EMPTY\" && test -z \"${{HOME+x}}\" && printf '%s' \"$VALUE\" > out/result",
            local_tmpdir().display()
        );
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
        // SURVEY3 S1: the id (the owner) is not keyed, the mnemonic is
        // (SpawnAction.computeKey).
        action.id += "-new";
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        action.extra.mnemonic = "Other".into();
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

    /// An entry an earlier binary wrote with a `native-action-v8` manifest
    /// is never trusted, even when it is self-consistent — the shape of an
    /// entry whose output was another action's (the stale-digest bug,
    /// docs/DIGEST_MEMO.md "Upgrade note"): the action runs again without an
    /// error, the entry is replaced by one of this format with the action's
    /// own bytes, and that one is a cache hit.
    #[test]
    fn entries_of_earlier_manifest_formats_are_executed_again() {
        let fixture = Fixture::new();
        let action = fixture.action();
        let first = execute(&fixture.cache(), &action).unwrap();
        let path = &first.outputs[0];
        let entry = path.parent().unwrap().parent().unwrap().parent().unwrap();
        let manifest_path = entry.join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(manifest["format"], MANIFEST_FORMAT);
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        // As the bug left it: another action's bytes, named by the manifest.
        let other = b"other action's bytes\n";
        fs::remove_file(path).unwrap();
        write_file(path, other, 0).unwrap();
        set_mode(path, OUTPUT_MODE).unwrap();
        manifest["format"] = "native-action-v8".into();
        manifest["outputs"][0]["digest"] = digest(other).hex().into();
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        // Accepted as it stands, it would be a hit with the wrong bytes.
        let mut current = manifest.clone();
        current["format"] = MANIFEST_FORMAT.into();
        fs::write(&manifest_path, serde_json::to_vec(&current).unwrap()).unwrap();
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        assert_eq!(fs::read(path).unwrap(), other);
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let again = execute(&fixture.cache(), &action).unwrap();
        assert!(!again.cache_hit);
        assert_eq!(again.outputs, first.outputs);
        assert_eq!(fs::read(path).unwrap(), b"hello\n");
        let rewritten: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(rewritten["format"], MANIFEST_FORMAT);
        assert_eq!(rewritten["outputs"][0]["digest"], digest(b"hello\n").hex());
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
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
            .push((fixture.0.join("also-missing").into(), "src/input".into()));
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
        action.inputs.push((identical.into(), "src/input".into()));
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        let conflicting = fixture.0.join("different");
        fs::write(&conflicting, "different").unwrap();
        action.inputs.push((conflicting.into(), "src/input".into()));
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
        action.inputs[0].0 = link.into();
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("symlink")
        );
        let dirlink = fixture.0.join("dirlink");
        symlink(&fixture.0, &dirlink).unwrap();
        action.inputs[0].0 = dirlink.join("source").into();
        assert!(execute(&fixture.cache(), &action).is_err());
        action.inputs[0].0 = fixture.0.clone().into();
        assert!(execute(&fixture.cache(), &action).is_err());
        action.inputs[0].0 = fixture.0.join("missing").into();
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
        consumer.inputs = vec![(tree.clone().into(), "in/tree".into())];
        consumer.extra.tree_inputs = BTreeSet::from([PathBuf::from("in/tree")]);
        consumer.command =
            "cat in/tree/a in/tree/sub/b > out/result; test -d in/tree/sub/empty".into();
        let consumed = execute(&fixture.cache(), &consumer).unwrap();
        assert_eq!(fs::read(&consumed.outputs[0]).unwrap(), b"ab");
        producer.command = producer.command.replace("printf b", "printf c");
        let changed = execute(&fixture.cache(), &producer).unwrap();
        consumer.inputs = vec![(changed.outputs[0].clone().into(), "in/tree".into())];
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

    /// Links that do not resolve to a regular file or directory fail the
    /// action with Bazel's validation errors
    /// (TreeArtifactValue.visitTree, reportOutputTreeArtifactErrors), the
    /// cycles at once; nothing is published.
    #[test]
    fn tree_outputs_reject_invalid_links_and_missing_directories() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.inputs.clear();
        action.outputs = vec!["bazel-out/k8-fastbuild/bin/pkg/tree".into()];
        action.extra.tree_outputs = BTreeSet::from([action.outputs[0].clone()]);
        let invalid = "error while validating output tree artifact pkg/tree: ";
        for (command, expected) in [
            ("ln -s missing $T/y", "child y is a dangling symbolic link"),
            (
                "mkdir $T/sub && ln -s ../../gone $T/sub/y",
                "child sub/y is a dangling symbolic link; its target '../../gone' lies outside the tree",
            ),
            (
                "ln -s /nonexistent/tree-target $T/y",
                "child y is a dangling symbolic link",
            ),
            // The action's work directory is gone once the action ran.
            (
                "printf t > ../t && ln -s \"$(cd .. && pwd)/t\" $T/y",
                "child y is a dangling symbolic link; its absolute target lies in the action's sandbox or temporary directory",
            ),
            (
                "ln -s /dev/null $T/null",
                "child null has an unsupported type",
            ),
            ("mkfifo $T/fifo", "child fifo has an unsupported type"),
            (
                "ln -s b $T/a && ln -s a $T/b",
                "/a (Too many levels of symbolic links)",
            ),
            (
                "printf r > $T/r && ln -s . $T/self",
                "/self (Too many levels of symbolic links)",
            ),
            (
                "mkdir $T/sub && ln -s .. $T/sub/up",
                "/sub/up (Too many levels of symbolic links)",
            ),
            (
                "mkdir $T/sub && ln -s ../.. $T/sub/out",
                "/sub/out/tree (Too many levels of symbolic links)",
            ),
        ] {
            action.command = format!("T=bazel-out/k8-fastbuild/bin/pkg/tree; {command}");
            let error = execute(&fixture.cache(), &action).unwrap_err();
            // ELOOP names the link's absolute path, like genuine's.
            let found = if expected.starts_with('/') {
                error.contains(&format!("{invalid}/")) && error.contains(expected)
            } else {
                error.contains(&format!("{invalid}{expected}"))
            };
            assert!(found, "{command}: {error}");
            fixture.assert_unpublished();
        }
        action.command = "rmdir bazel-out/k8-fastbuild/bin/pkg/tree && printf x > bazel-out/k8-fastbuild/bin/pkg/tree".into();
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("expected directory")
        );
        fixture.assert_unpublished();
    }

    fn entries(tree: &Path) -> Vec<(String, char, u32)> {
        let mut found = Vec::new();
        let mut pending = vec![PathBuf::new()];
        while let Some(relative) = pending.pop() {
            for entry in fs::read_dir(tree.join(&relative)).unwrap() {
                let child = relative.join(entry.unwrap().file_name());
                let metadata = fs::symlink_metadata(tree.join(&child)).unwrap();
                let kind = if metadata.file_type().is_symlink() {
                    'l'
                } else if metadata.is_dir() {
                    pending.push(child.clone());
                    'd'
                } else {
                    'f'
                };
                found.push((child.display().to_string(), kind, mode(&tree.join(&child))));
            }
        }
        found.sort();
        found
    }

    /// The rules_oci shapes and every link that resolves: the tree's files
    /// are what the links resolve to at the links' paths (with the target's
    /// executable bits, never chmod'ed), its directories include linked
    /// ones; the published tree holds them as store inodes and real
    /// directories (no links), is valid on reuse and is rebuilt when damaged;
    /// a consumer stages exactly that.
    #[test]
    fn tree_outputs_follow_symlinks_and_publish_what_they_resolve_to() {
        let fixture = Fixture::new();
        let source = fixture.0.join("data.txt");
        fs::write(&source, b"data\n").unwrap();
        fs::set_permissions(&source, fs::Permissions::from_mode(0o644)).unwrap();
        let tool = fixture.0.join("tool.sh");
        write_executable_script(&tool, "#!/bin/sh\necho tool\n", 0o755);
        let host = fixture.0.join("host.txt");
        fs::write(&host, b"host\n").unwrap();
        fs::set_permissions(&host, fs::Permissions::from_mode(0o600)).unwrap();
        let mut producer = fixture.action();
        producer.inputs = vec![
            (source.clone().into(), "pkg/data.txt".into()),
            (tool.clone().into(), "pkg/tool.sh".into()),
        ];
        let tree_path = PathBuf::from("bazel-out/k8-fastbuild/bin/pkg/image");
        producer.outputs = vec![tree_path.clone()];
        producer.extra.tree_outputs = BTreeSet::from([tree_path.clone()]);
        producer.command = format!(
            "set -e; T={}; mkdir -p $T/blobs/sha256 $T/real/deeper $T/empty
ln -s \"$(realpath --relative-to=$T/blobs/sha256 pkg/data.txt --no-symlinks)\" $T/blobs/sha256/layer
ln -s ../../../../../pkg/tool.sh $T/tool
printf a > $T/a.txt; chmod 644 $T/a.txt; ln -s a.txt $T/alias.txt; ln -s alias.txt $T/chain.txt
printf f > $T/real/f; printf g > $T/real/deeper/g; ln -s f $T/real/f2; ln -s real $T/alias
ln -s empty $T/empty_alias; ln -s '{}' $T/host",
            tree_path.display(),
            host.display()
        );
        let first = execute(&fixture.cache(), &producer).unwrap();
        let tree = &first.outputs[0];
        let names = [
            "a.txt",
            "alias.txt",
            "alias/deeper/g",
            "alias/f",
            "alias/f2",
            "blobs/sha256/layer",
            "chain.txt",
            "host",
            "real/deeper/g",
            "real/f",
            "real/f2",
            "tool",
        ];
        assert_eq!(first.trees[&0], names.map(PathBuf::from));
        // Published: no links, directories real, files their blobs with
        // the resolved file's executable bits.
        let directory = |name: &str| (name.to_owned(), 'd', OUTPUT_MODE);
        let file = |name: &str, mode: u32| (name.to_owned(), 'f', mode);
        assert_eq!(
            entries(tree),
            [
                file("a.txt", OUTPUT_MODE),
                directory("alias"),
                file("alias.txt", OUTPUT_MODE),
                directory("alias/deeper"),
                file("alias/deeper/g", OUTPUT_MODE),
                file("alias/f", OUTPUT_MODE),
                file("alias/f2", OUTPUT_MODE),
                directory("blobs"),
                directory("blobs/sha256"),
                file("blobs/sha256/layer", 0o444),
                file("chain.txt", OUTPUT_MODE),
                directory("empty"),
                directory("empty_alias"),
                file("host", 0o444),
                directory("real"),
                directory("real/deeper"),
                file("real/deeper/g", OUTPUT_MODE),
                file("real/f", OUTPUT_MODE),
                file("real/f2", OUTPUT_MODE),
                file("tool", OUTPUT_MODE),
            ]
        );
        for (name, content) in [
            ("blobs/sha256/layer", &b"data\n"[..]),
            ("tool", b"#!/bin/sh\necho tool\n"),
            ("chain.txt", b"a"),
            ("alias/f2", b"f"),
            ("host", b"host\n"),
        ] {
            assert_eq!(fs::read(tree.join(name)).unwrap(), content, "{name}");
        }
        assert_eq!(
            ino(&tree.join("blobs/sha256/layer")),
            ino(&blob(&fixture, b"data\n", 0o444))
        );
        assert_eq!(ino(&tree.join("alias/f")), ino(&tree.join("real/f")));
        // Neither the link targets outside the tree nor its sources changed.
        assert_eq!(mode(&host), 0o600);
        assert_eq!(mode(&source), 0o644);
        // The manifest records the modes that are not OUTPUT_MODE.
        let manifest: Manifest = serde_json::from_slice(
            &fs::read(tree.ancestors().nth(6).unwrap().join("manifest.json")).unwrap(),
        )
        .unwrap();
        let recorded = &manifest.outputs[0].tree.as_ref().unwrap().modes;
        assert_eq!(
            recorded,
            &[(hex(b"blobs/sha256/layer"), 0o444), (hex(b"host"), 0o444)]
        );
        // The memo holds what a fresh read of the published tree finds.
        let root = fs::canonicalize(fixture.cache()).unwrap();
        let fresh = read_tree(tree, Some(&root), TreeRead::Published).unwrap();
        assert!(*memoized_tree(tree).unwrap() == fresh);
        assert!(fresh.links.is_empty());
        let hit = execute(&fixture.cache(), &producer).unwrap();
        assert!(hit.cache_hit);
        assert_eq!(hit.trees, first.trees);
        // A consumer stages the tree: the linked files at their paths.
        let mut consumer = fixture.action();
        consumer.id = "consumer".into();
        consumer.inputs = vec![(tree.clone().into(), "in/image".into())];
        consumer.extra.tree_inputs = BTreeSet::from([PathBuf::from("in/image")]);
        consumer.command = "cd in/image; for f in blobs/sha256/layer tool alias/deeper/g host; do \
             if [ -x $f ]; then x=x; else x=-; fi; echo \"$f $x $(cat $f)\"; done > ../../out/result; \
             test -d empty_alias && test ! -L alias"
            .into();
        let consumed = execute(&fixture.cache(), &consumer).unwrap();
        assert_eq!(
            fs::read_to_string(&consumed.outputs[0]).unwrap(),
            "blobs/sha256/layer - data\ntool x #!/bin/sh\necho tool\nalias/deeper/g x g\nhost - host\n"
        );
        // Damage is found on reuse: a changed mode, a link again.
        set_mode(&tree.join("blobs/sha256"), 0o755).unwrap();
        set_mode(&tree.join("blobs/sha256/layer"), 0o555).unwrap();
        assert!(!execute(&fixture.cache(), &producer).unwrap().cache_hit);
        assert_eq!(mode(&tree.join("blobs/sha256/layer")), 0o444);
        set_mode(tree, 0o755).unwrap();
        fs::remove_file(tree.join("a.txt")).unwrap();
        symlink("real/f", tree.join("a.txt")).unwrap();
        set_mode(tree, TREE_ROOT_MODE).unwrap();
        assert!(!execute(&fixture.cache(), &producer).unwrap().cache_hit);
        assert_eq!(entries(tree)[0], file("a.txt", OUTPUT_MODE));
        assert!(execute(&fixture.cache(), &consumer).unwrap().cache_hit);
    }

    /// A link to another output of the same action (a file, or a file of
    /// another tree output) shows that output as published (OUTPUT_MODE),
    /// whatever the order of the outputs.
    #[test]
    fn tree_links_to_other_outputs_see_their_final_mode() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.inputs.clear();
        action.outputs = vec!["out/a".into(), "out/b".into(), "out/z.txt".into()];
        action.extra.tree_outputs =
            BTreeSet::from([PathBuf::from("out/a"), PathBuf::from("out/b")]);
        // out/b also links into out/a, which is published first.
        action.command = "printf z > out/z.txt; chmod 644 out/z.txt; printf b > out/b/own; chmod 600 out/b/own; \
             printf a > out/a/own; ln -s ../z.txt out/a/z; ln -s ../b/own out/a/b; ln -s ../a/z out/b/z; \
             ln -s ../a/own out/b/a"
            .into();
        let result = execute(&fixture.cache(), &action).unwrap();
        for (tree, name, content) in [
            (0, "z", "z"),
            (0, "b", "b"),
            (1, "z", "z"),
            (1, "own", "b"),
            (1, "a", "a"),
        ] {
            let path = result.outputs[tree].join(name);
            assert_eq!(fs::read_to_string(&path).unwrap(), content);
            assert_eq!(mode(&path), OUTPUT_MODE, "{}", path.display());
        }
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
    }

    /// A runfile read by content (a test's) is a file, or a tree's files
    /// (none for an empty tree), also through a symlink output of a tree.
    #[test]
    fn content_sources_are_files_or_the_files_of_trees() {
        let fixture = Fixture::new();
        let mut producer = fixture.action();
        producer.inputs.clear();
        producer.outputs = vec!["out/tree".into(), "out/empty".into(), "out/file".into()];
        producer.extra.tree_outputs =
            BTreeSet::from([PathBuf::from("out/tree"), PathBuf::from("out/empty")]);
        producer.command =
            "printf a > out/tree/a; mkdir out/tree/d; ln -s ../a out/tree/d/b; printf f > out/file"
                .into();
        let produced = execute(&fixture.cache(), &producer).unwrap();
        let mut link = fixture.action();
        link.id = "tree-link".into();
        link.command.clear();
        link.inputs = vec![(produced.outputs[0].clone().into(), "in/tree".into())];
        link.extra.tree_inputs = BTreeSet::from([PathBuf::from("in/tree")]);
        link.outputs = vec!["out/link".into()];
        link.extra.tree_outputs = BTreeSet::from([PathBuf::from("out/link")]);
        link.extra.transform = Some(Transform::Symlink {
            target: "in/tree".into(),
            require_executable: false,
            progress: "Creating symlink".into(),
        });
        let linked = execute(&fixture.cache(), &link).unwrap();
        let digest_a = digest(b"a").hex();
        for tree in [&produced.outputs[0], &linked.outputs[0]] {
            let ContentSource::Tree(files) = read_content_source(&fixture.cache(), tree).unwrap()
            else {
                panic!("a tree");
            };
            let files: Vec<(PathBuf, String, bool)> = files
                .into_iter()
                .map(|(name, input)| (name, input.digest(), input.executable()))
                .collect();
            assert_eq!(
                files,
                [
                    (PathBuf::from("a"), digest_a.clone(), true),
                    (PathBuf::from("d/b"), digest_a.clone(), true)
                ]
            );
        }
        let ContentSource::Tree(files) =
            read_content_source(&fixture.cache(), &produced.outputs[1]).unwrap()
        else {
            panic!("an empty tree");
        };
        assert!(files.is_empty());
        let ContentSource::File(file) =
            read_content_source(&fixture.cache(), &produced.outputs[2]).unwrap()
        else {
            panic!("a file");
        };
        assert_eq!(file.digest(), digest(b"f").hex());
    }

    /// A tree input that is no published output (here a directory of the
    /// fixture) is read with its links followed; a published tree has none.
    #[test]
    fn tree_inputs_follow_links_and_published_trees_reject_them() {
        let fixture = Fixture::new();
        let tree = fixture.0.join("dir");
        fs::create_dir_all(tree.join("sub")).unwrap();
        fs::write(tree.join("sub/x"), b"x").unwrap();
        symlink("sub", tree.join("link")).unwrap();
        let read = read_tree(&tree, None, TreeRead::Input).unwrap();
        assert_eq!(
            read.files.keys().collect::<Vec<_>>(),
            [Path::new("link/x"), Path::new("sub/x")]
        );
        assert_eq!(read.files[Path::new("link/x")].path, tree.join("sub/x"));
        assert_eq!(read.links, BTreeSet::from([PathBuf::from("link")]));
        let Err(error) = read_tree(&tree, None, TreeRead::Published) else {
            panic!("a link in a published tree");
        };
        assert_eq!(
            error,
            format!(
                "published tree artifact entry '{}' is a symlink",
                tree.join("link").display()
            )
        );
        symlink("gone", tree.join("sub/y")).unwrap();
        let Err(error) = read_tree(&tree, None, TreeRead::Input) else {
            panic!("a dangling link");
        };
        // The walk reaches `sub` before the link to it.
        assert_eq!(
            error,
            format!(
                "tree '{}': child sub/y is a dangling symbolic link",
                tree.display()
            )
        );
    }

    /// Serializes the tests that forget the process-wide source directory
    /// memo, so that one test's forgetting never interleaves with another's
    /// reads.
    fn source_directory_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Forget the per-process source directory memo (a command reads a
    /// source directory once; these tests change one within the process).
    fn forget_source_directories() {
        let mut memo = source_directory_memo()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        memo.slots.clear();
        memo.entries = 0;
    }

    /// A source directory with files (one executable), a subdirectory, an
    /// empty directory, links to a file and a directory and a dangling link.
    fn source_tree(root: &Path) -> PathBuf {
        let dir = root.join("srcdir");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::create_dir_all(dir.join("empty")).unwrap();
        fs::write(dir.join("a.txt"), "a\n").unwrap();
        fs::set_permissions(dir.join("a.txt"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(dir.join("sub/b.txt"), "b\n").unwrap();
        fs::write(dir.join("x.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(dir.join("x.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("a.txt", dir.join("link_a")).unwrap();
        std::os::unix::fs::symlink("sub", dir.join("link_sub")).unwrap();
        std::os::unix::fs::symlink("missing.txt", dir.join("dangling")).unwrap();
        dir
    }

    #[test]
    fn source_directory_inputs_are_staged_and_keyed_by_content() {
        let _lock = source_directory_test_lock();
        let fixture = Fixture::new();
        let dir = source_tree(&fixture.0);
        let mut action = fixture.action();
        action.inputs = vec![(dir.clone().into(), "in/dir".into())];
        action.extra.source_directories = BTreeSet::from([PathBuf::from("in/dir")]);
        action.command = [
            "cd in/dir",
            "test ! -L . && test ! -L link_a && test ! -L link_sub",
            "test -x x.sh && test -d empty && test -L dangling",
            "{ cat a.txt link_a sub/b.txt link_sub/b.txt; readlink dangling; test -x a.txt && echo x || echo -; } > ../../out/result",
        ]
        .join(" && ");
        let first = execute(&fixture.cache(), &action).unwrap();
        assert!(!first.cache_hit);
        assert_eq!(
            fs::read(&first.outputs[0]).unwrap(),
            b"a\na\nb\nb\nmissing.txt\n-\n"
        );
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        // A new process sees an unchanged directory: the same key.
        forget_source_directories();
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        // touch changes no content.
        let file = fs::File::open(dir.join("sub/b.txt")).unwrap();
        file.set_modified(SystemTime::now()).unwrap();
        forget_source_directories();
        assert!(execute(&fixture.cache(), &action).unwrap().cache_hit);
        // Content, an empty directory, a link's target text, executable
        // bits: each is a new key.
        let changes: [&dyn Fn(); 4] = [
            &|| fs::write(dir.join("sub/b.txt"), "c\n").unwrap(),
            &|| fs::create_dir(dir.join("empty/inner")).unwrap(),
            &|| {
                fs::remove_file(dir.join("dangling")).unwrap();
                std::os::unix::fs::symlink("other.txt", dir.join("dangling")).unwrap();
            },
            &|| fs::set_permissions(dir.join("a.txt"), fs::Permissions::from_mode(0o755)).unwrap(),
        ];
        for (index, change) in changes.iter().enumerate() {
            change();
            forget_source_directories();
            assert!(
                !execute(&fixture.cache(), &action).unwrap().cache_hit,
                "change {index}"
            );
        }
        let result = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(
            fs::read(&result.outputs[0]).unwrap(),
            b"a\na\nc\nc\nother.txt\nx\n"
        );
        // Within one process the first read is kept, like Bazel's per-build
        // value: a later change is not seen until the next command.
        fs::set_permissions(dir.join("a.txt"), fs::Permissions::from_mode(0o644)).unwrap();
        forget_source_directories();
        let kept = execute(&fixture.cache(), &action).unwrap();
        fs::write(dir.join("a.txt"), "changed\n").unwrap();
        let again = execute(&fixture.cache(), &action).unwrap();
        assert!(again.cache_hit);
        assert_eq!(again.outputs, kept.outputs);
        // Without the source directory flag a directory input is rejected.
        action.extra.source_directories.clear();
        assert!(
            execute(&fixture.cache(), &action)
                .unwrap_err()
                .contains("expected regular file")
        );
        forget_source_directories();
    }

    #[test]
    fn source_directory_reads_fail_on_link_cycles_and_special_entries() {
        let fixture = Fixture::new();
        let dir = fixture.0.join("loop");
        fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink("..", dir.join("up")).unwrap();
        let error = read_source_directory(&dir, None).err().unwrap();
        assert!(
            error.ends_with("up': Infinite symlink expansion"),
            "{error}"
        );
        let cycle = fixture.0.join("cycle");
        fs::create_dir_all(&cycle).unwrap();
        std::os::unix::fs::symlink("b", cycle.join("a")).unwrap();
        std::os::unix::fs::symlink("a", cycle.join("b")).unwrap();
        let error = read_source_directory(&cycle, None).err().unwrap();
        assert!(error.ends_with("a': Symlink cycle"), "{error}");
        let special = fixture.0.join("special");
        fs::create_dir_all(&special).unwrap();
        let _socket = std::os::unix::net::UnixListener::bind(special.join("s")).unwrap();
        let error = read_source_directory(&special, None).err().unwrap();
        assert!(
            error.contains("is not a regular file, directory or symlink"),
            "{error}"
        );
        // A directory through a linked ancestor is no source directory path.
        std::os::unix::fs::symlink(&dir, fixture.0.join("linked")).unwrap();
        let error = read_source_directory(&fixture.0.join("linked/sub"), None)
            .err()
            .unwrap();
        assert!(error.contains("symlink is not allowed"), "{error}");
    }

    /// MerkleTreeComputer.explodeDirectory: a source directory enters the
    /// remote input root as its files at their paths (links followed, no
    /// empty directories), exactly like those files as separate inputs; a
    /// dangling link is not supported.
    #[test]
    fn source_directories_enter_the_remote_input_root_as_their_files() {
        let _lock = source_directory_test_lock();
        let fixture = Fixture::new();
        let dir = source_tree(&fixture.0);
        fs::remove_file(dir.join("dangling")).unwrap();
        let mut action = fixture.action();
        action.inputs = vec![(dir.clone().into(), "in/dir".into())];
        action.extra.source_directories = BTreeSet::from([PathBuf::from("in/dir")]);
        let directory_key = remote_action(&action, &prepare(&action, None).unwrap(), true, &[])
            .unwrap()
            .0
            .key;
        let mut files = fixture.action();
        files.inputs = [
            ("a.txt", "a.txt"),
            ("a.txt", "link_a"),
            ("sub/b.txt", "sub/b.txt"),
            ("sub/b.txt", "link_sub/b.txt"),
            ("x.sh", "x.sh"),
        ]
        .iter()
        .map(|(source, name)| {
            (
                dir.join(source).into(),
                Path::new("in/dir").join(name).into(),
            )
        })
        .collect();
        let files_key = remote_action(&files, &prepare(&files, None).unwrap(), true, &[])
            .unwrap()
            .0
            .key;
        assert_eq!(directory_key, files_key);
        std::os::unix::fs::symlink("missing.txt", dir.join("dangling")).unwrap();
        forget_source_directories();
        let error = remote_action(&action, &prepare(&action, None).unwrap(), true, &[])
            .err()
            .unwrap();
        assert_eq!(
            error,
            "The file type of 'in/dir/dangling' is not supported."
        );
        forget_source_directories();
    }

    #[test]
    fn content_inputs_read_and_stage_source_directories() {
        let _lock = source_directory_test_lock();
        let fixture = Fixture::new();
        let dir = source_tree(&fixture.0);
        let input = read_content_input(&fixture.cache(), &dir).unwrap();
        assert!(!input.executable());
        let digest = input.digest();
        let execroot = fixture.0.join("execroot");
        stage_content_inputs(&fixture.cache(), &[(input, "r/dir".into())], &execroot).unwrap();
        assert_eq!(
            fs::read(execroot.join("r/dir/link_sub/b.txt")).unwrap(),
            b"b\n"
        );
        assert!(execroot.join("r/dir/empty").is_dir());
        assert_eq!(
            fs::read_link(execroot.join("r/dir/dangling")).unwrap(),
            Path::new("missing.txt")
        );
        fs::write(dir.join("a.txt"), "changed\n").unwrap();
        forget_source_directories();
        assert_ne!(
            read_content_input(&fixture.cache(), &dir).unwrap().digest(),
            digest
        );
        forget_source_directories();
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

    /// docs/HARDLINK_STAGING.md, "Cache checks of published trees": a
    /// valid tree's settled directories are stamped next to its manifest;
    /// later checks inspect only the directories (no walk), take the files
    /// from the manifest and the stamp, and any entry added, removed or
    /// renamed in any directory of the tree fails the stamp: the full
    /// check finds the damage and the entry is published again.
    #[test]
    fn published_trees_are_checked_by_their_directory_stamps() {
        let fixture = Fixture::new();
        let mut producer = fixture.action();
        producer.inputs.clear();
        producer.outputs = vec!["out/tree".into()];
        producer.extra.tree_outputs = BTreeSet::from([PathBuf::from("out/tree")]);
        producer.command =
            "mkdir -p out/tree/sub/deep && printf a > out/tree/a && printf b > out/tree/sub/b && printf c > out/tree/sub/deep/c".into();
        let first = execute(&fixture.cache(), &producer).unwrap();
        let tree = first.outputs[0].clone();
        let entry = tree.ancestors().nth(3).unwrap().to_path_buf();
        let stamps = entry.join(tree_stamps::FILE_NAME);
        // Directories just written are not settled: no stamp.
        assert!(execute(&fixture.cache(), &producer).unwrap().cache_hit);
        assert!(!stamps.exists());
        crate::dir_stamps::SETTLE_ALL.with(|settle| settle.set(true));
        let walks = tree_walks();
        let hit = execute(&fixture.cache(), &producer).unwrap();
        assert!(hit.cache_hit);
        assert!(stamps.exists());
        // The unlocked check found the stamps missing, the locked one
        // wrote them: two walks.
        assert_eq!(tree_walks(), walks + 2);
        let stamped = fs::metadata(&stamps).unwrap().modified().unwrap();
        let root = fs::canonicalize(fixture.cache()).unwrap();
        let walked = read_tree(&tree, Some(&root), TreeRead::Published).unwrap();
        for _ in 0..2 {
            let walks = tree_walks();
            let again = execute(&fixture.cache(), &producer).unwrap();
            assert!(again.cache_hit);
            assert_eq!(again.trees, first.trees);
            // No walk, nothing written, and the stamped tree is exactly the
            // walked one (paths, digests, sizes and modes).
            assert_eq!(tree_walks(), walks);
            assert_eq!(fs::metadata(&stamps).unwrap().modified().unwrap(), stamped);
            let memoized = memoized_tree(&tree).unwrap();
            assert!(*memoized == walked);
            assert_eq!(tree_paths(&memoized), tree_paths(&walked));
            for (name, input) in &walked.files {
                assert_eq!(memoized.files[name].size, input.size, "{}", name.display());
            }
        }
        // Damage in any directory of the tree, at any depth, is caught.
        type Damage<'a> = (&'a str, &'a dyn Fn(&Path));
        let damage: [Damage; 4] = [
            ("sub/deep", &|dir| {
                fs::write(dir.join("extra"), "x").unwrap()
            }),
            ("sub", &|dir| fs::remove_file(dir.join("b")).unwrap()),
            ("", &|dir| {
                fs::rename(dir.join("a"), dir.join("renamed")).unwrap()
            }),
            ("sub/deep", &|dir| {
                fs::remove_file(dir.join("c")).unwrap();
                fs::write(dir.join("c"), "c").unwrap();
            }),
        ];
        for (directory, damage) in damage {
            let directory = tree.join(directory);
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
            damage(&directory);
            fs::set_permissions(&directory, fs::Permissions::from_mode(OUTPUT_MODE)).unwrap();
            let rebuilt = execute(&fixture.cache(), &producer).unwrap();
            assert!(!rebuilt.cache_hit, "{}", directory.display());
            assert_eq!(fs::read(tree.join("sub/deep/c")).unwrap(), b"c");
            assert!(!tree.join("sub/deep/extra").exists());
            // The republished tree is stamped again by a later check.
            assert!(execute(&fixture.cache(), &producer).unwrap().cache_hit);
            assert!(stamps.exists());
        }
        // A stamp file of another manifest, or a corrupt one, is ignored.
        fs::write(&stamps, b"BZRSTSTP corrupt").unwrap();
        let walks = tree_walks();
        assert!(execute(&fixture.cache(), &producer).unwrap().cache_hit);
        assert_eq!(tree_walks(), walks + 2);
        let walks = tree_walks();
        assert!(execute(&fixture.cache(), &producer).unwrap().cache_hit);
        assert_eq!(tree_walks(), walks);
        crate::dir_stamps::SETTLE_ALL.with(|settle| settle.set(false));
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
        let fresh = read_tree(&tree, Some(&root), TreeRead::Published).unwrap();
        assert!(*seeded == fresh);
        assert_eq!(tree_paths(&seeded), tree_paths(&fresh));
        let consumer = |id: &str| {
            let mut consumer = fixture.action();
            consumer.id = id.into();
            // Keyed apart (the id is not keyed, SURVEY3 S1).
            consumer.extra.mnemonic = id.into();
            consumer.inputs = vec![(tree.clone().into(), "in/tree".into())];
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
        assert!(*republished == read_tree(&tree, Some(&root), TreeRead::Published).unwrap());
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
        let read = read_tree(&tree, None, TreeRead::Input).unwrap();
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
                    tree.join(format!("f{i:03}")).into(),
                    PathBuf::from(format!("in/{i}")).into(),
                )
            })
            .collect();
        action
            .inputs
            .push((tree.join("f001").into(), "in/0".into()));
        action.inputs[100].0 = fixture.0.join("missing").into();
        let Err(error) = prepare(&action, None) else {
            panic!("missing input");
        };
        assert!(error.starts_with("cannot inspect") && error.contains("missing"));
        action.inputs[100].0 = tree.join("f100").into();
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
            // A link is a walk failure of a published tree.
            let Err(error) = read_tree(&tree, None, TreeRead::Published) else {
                panic!("unreadable file");
            };
            assert!(
                error.starts_with(&format!("cannot read '{}'", tree.join("f150").display())),
                "{error}"
            );
            fs::set_permissions(tree.join("f150"), fs::Permissions::from_mode(0o644)).unwrap();
        }
        let Err(error) = read_tree(&tree, None, TreeRead::Published) else {
            panic!("symlink in tree");
        };
        assert!(
            error.starts_with(&format!(
                "published tree artifact entry '{}' is a symlink",
                tree.join("sub/link").display()
            )),
            "{error}"
        );
        // Anywhere else the link is followed.
        let followed = read_tree(&tree, None, TreeRead::Input).unwrap();
        assert_eq!(followed.files.len(), 302);
        assert_eq!(
            followed.files[Path::new("sub/link")].path,
            tree.join("sub/x")
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
            root.join("repositories/external/+r+n/file"),
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
            "/repositories/external/+r+n/file",
            "/repositories/external/trees/x/file",
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
        // Paths outside the cache take their own memo (source_directories):
        // no full walk, and their directories once per process.
        let source = fixture.0.join("sources/pkg/source");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, b"x").unwrap();
        let before = full_ancestor_walks();
        for _ in 0..2 {
            assert!(no_linked_ancestor(&source, Some(&root)).unwrap().is_file());
        }
        assert_eq!(full_ancestor_walks(), before);
        assert!(
            source_directories()
                .read()
                .unwrap()
                .contains(source.parent().unwrap())
        );
        // A source below a linked directory fails exactly as before.
        symlink(source.parent().unwrap(), fixture.0.join("sources/linked")).unwrap();
        let linked = fixture.0.join("sources/linked/source");
        assert_eq!(
            no_linked_ancestor(&linked, Some(&root)).unwrap_err(),
            no_linked_ancestor(&linked, None).unwrap_err()
        );
        // Everything else takes the full walk: the execution roots, other
        // spellings, and calls without a cache root.
        let work = root.join("work/x");
        fs::create_dir_all(&work).unwrap();
        fs::write(work.join("file"), b"x").unwrap();
        for (path, cache) in [
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
            "repositories/external/+r+n",
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
            for shard in verified_directories().all() {
                shard.write().unwrap().clear();
            }
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
            ("repositories/external/+r+n/file", false, false),
            ("repositories/external/trees/x/file", false, false),
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
            ("digests/v2.dmemo", false, false),
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

    /// UnresolvedSymlinkAction: one symlink output, no inputs, keyed by the
    /// literal target and its type; it creates the symlink with the
    /// normalized link text, published as is and validated by its text;
    /// consumers stage the literal symlink and key its text; a spawn's
    /// declared symlink output must be a symlink.
    #[test]
    fn unresolved_symlinks_are_created_published_and_staged() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.command.clear();
        action.inputs.clear();
        action.extra.transform = Some(Transform::UnresolvedSymlink {
            target: "../x".into(),
            target_type: SymlinkTargetType::Unspecified,
            progress: "Creating symlink out/result".into(),
        });
        assert!(
            validate_structure(&action)
                .unwrap_err()
                .contains("output must be a symlink artifact")
        );
        action.extra.symlink_outputs = action.outputs.iter().cloned().collect();
        validate_structure(&action).unwrap();
        let key = |action: &Action| prepare(action, None).unwrap().key;
        let unspecified = key(&action);
        let mut directory = action.clone();
        directory.extra.transform = Some(Transform::UnresolvedSymlink {
            target: "../x".into(),
            target_type: SymlinkTargetType::Directory,
            progress: String::new(),
        });
        let mut other = action.clone();
        other.extra.transform = Some(Transform::UnresolvedSymlink {
            target: "./a//b/../../../y/".into(),
            target_type: SymlinkTargetType::Unspecified,
            progress: "Creating symlink out/result".into(),
        });
        assert_ne!(unspecified, key(&directory));
        assert_ne!(unspecified, key(&other));
        // Created dangling, published as the symlink itself.
        let first = execute(&fixture.cache(), &action).unwrap();
        assert!(!first.cache_hit);
        assert_eq!(fs::read_link(&first.outputs[0]).unwrap(), Path::new("../x"));
        let again = execute(&fixture.cache(), &action).unwrap();
        assert!(again.cache_hit);
        assert_eq!(again.outputs, first.outputs);
        // A published link with another text is no longer the entry.
        let published = &first.outputs[0];
        fs::remove_file(published).unwrap();
        symlink("../z", published).unwrap();
        assert!(!execute(&fixture.cache(), &action).unwrap().cache_hit);
        assert_eq!(fs::read_link(published).unwrap(), Path::new("../x"));
        // PathFragment.create normalizes the text.
        let normalized = execute(&fixture.cache(), &other).unwrap();
        assert_eq!(
            fs::read_link(&normalized.outputs[0]).unwrap(),
            Path::new("../y")
        );
        // A consumer stages the literal symlink and keys its text.
        let mut consumer = fixture.action();
        consumer.id = "consumer".into();
        consumer.inputs = vec![(first.outputs[0].clone().into(), "pkg/link".into())];
        consumer.command = "readlink pkg/link > out/result; test -L pkg/link".into();
        consumer.extra.symlink_inputs = BTreeSet::from([PathBuf::from("pkg/link")]);
        let read = execute(&fixture.cache(), &consumer).unwrap();
        assert_eq!(fs::read(&read.outputs[0]).unwrap(), b"../x\n");
        let mut retargeted = consumer.clone();
        retargeted.inputs = vec![(normalized.outputs[0].clone().into(), "pkg/link".into())];
        assert_ne!(key(&consumer), key(&retargeted));
        let read = execute(&fixture.cache(), &retargeted).unwrap();
        assert_eq!(fs::read(&read.outputs[0]).unwrap(), b"../y\n");
        // A regular file is no unresolved symlink input.
        let mut regular = consumer.clone();
        regular.inputs = vec![(fixture.0.join("source").into(), "pkg/link".into())];
        assert!(
            execute(&fixture.cache(), &regular)
                .unwrap_err()
                .contains("is not a symlink")
        );
        // Runfiles transforms reproduce the link text.
        let manifest = Action {
            id: "manifest".into(),
            inputs: vec![(first.outputs[0].clone().into(), "pkg/link".into())],
            outputs: vec!["bin/b.runfiles_manifest".into()],
            extra: ActionExtra {
                transform: Some(Transform::RunfilesManifest {
                    entries: vec![("_main/pkg/link".into(), Some("pkg/link".into()))].into(),
                }),
                symlink_inputs: BTreeSet::from([PathBuf::from("pkg/link")]),
                ..ActionExtra::default()
            },
            ..Action::default()
        };
        let written = execute(&fixture.cache(), &manifest).unwrap();
        assert_eq!(
            fs::read(&written.outputs[0]).unwrap(),
            b"_main/pkg/link ../x\n"
        );
        // A spawn's declared symlink output: created by the process, or
        // `declared output '...' is not a symlink`.
        let mut spawn = fixture.action();
        spawn.id = "spawn".into();
        spawn.command = "ln -s data.txt out/result".into();
        spawn.extra.symlink_outputs = spawn.outputs.iter().cloned().collect();
        let linked = execute(&fixture.cache(), &spawn).unwrap();
        assert_eq!(
            fs::read_link(&linked.outputs[0]).unwrap(),
            Path::new("data.txt")
        );
        spawn.command = "cat src/input > out/result".into();
        assert!(
            execute(&fixture.cache(), &spawn)
                .unwrap_err()
                .contains("declared output 'out/result' is not a symlink")
        );
    }

    /// PathFragment.create's normalization and the digest of
    /// UnresolvedSymlinkArtifactValue.
    #[test]
    fn unresolved_link_text_and_digest_follow_bazel() {
        for (target, text) in [
            ("data.txt", "data.txt"),
            ("../../../../data.txt", "../../../../data.txt"),
            ("a/./b//c/", "a/b/c"),
            ("a/../b", "b"),
            ("a/../..", ".."),
            ("../a/../b", "../b"),
            ("/../x/./y/..", "/x"),
            ("/", "/"),
            (".", ""),
            ("", ""),
        ] {
            assert_eq!(unresolved_link_text(target), text, "{target}");
        }
        let mut expected: [u8; 32] = Sha256::digest(b"data.txt").into();
        expected[0] ^= 0xff;
        assert_eq!(unresolved_symlink_digest(b"data.txt").0, expected);
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
        consumer.inputs = vec![(a.outputs[0].clone().into(), "generated".into())];
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

    /// Distinct artifacts with one content are distinct inodes inside an
    /// action (Bazel links each input to its own file); one artifact at
    /// several paths is one inode. The further artifacts are staged from
    /// persistent distinct copies of the blob, reused by later actions and
    /// replaced when damaged, like blobs.
    #[test]
    fn distinct_artifacts_with_one_content_are_staged_as_distinct_inodes() {
        let fixture = Fixture::new();
        let mut action = fixture.action();
        let first = action.inputs[0].0.clone();
        let second = fixture.0.join("second");
        let third = fixture.0.join("third");
        fs::write(&second, b"hello\n").unwrap();
        fs::write(&third, b"hello\n").unwrap();
        action.inputs = vec![
            (first.clone(), "a".into()),
            (first, "again/a".into()),
            (second.into(), "b".into()),
            (third.into(), "c".into()),
        ];
        action.command = "stat -L -c %i a again/a b c > out/result".into();
        let result = execute(&fixture.cache(), &action).unwrap();
        let text = fs::read_to_string(&result.outputs[0]).unwrap();
        let inodes: Vec<&str> = text.lines().collect();
        let store = blob(&fixture, b"hello\n", 0o444);
        let copy = |n: usize| PathBuf::from(format!("{}.{n}", store.display()));
        assert_eq!(
            inodes,
            [ino(&store), ino(&store), ino(&copy(1)), ino(&copy(2))].map(|inode| inode.to_string())
        );
        assert_eq!(mode(&copy(2)), 0o444);
        // Another action reuses the copies.
        let (one, two) = (ino(&copy(1)), ino(&copy(2)));
        action.id = "again".into();
        action.command.push_str("; true");
        execute(&fixture.cache(), &action).unwrap();
        assert_eq!((ino(&copy(1)), ino(&copy(2))), (one, two));
        // A damaged copy is replaced.
        fs::set_permissions(copy(1), fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(copy(1), b"evil!!").unwrap();
        fs::set_permissions(copy(1), fs::Permissions::from_mode(0o444)).unwrap();
        action.id = "third".into();
        action.command.push_str("; cat b >> out/result");
        let result = execute(&fixture.cache(), &action).unwrap();
        assert!(
            fs::read_to_string(&result.outputs[0])
                .unwrap()
                .ends_with("\nhello\n")
        );
        assert_eq!(fs::read(copy(1)).unwrap(), b"hello\n");
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
        link.inputs = vec![(header.clone().into(), "pkg/hdr.h".into())];
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
            linked.outputs[0].clone().into(),
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
            (header.clone().into(), "pkg/hdr.h".into()),
            (
                linked.outputs[0].clone().into(),
                "pkg/_virtual_includes/hdr.h".into(),
            ),
            (chained.outputs[0].clone().into(), "pkg/again.h".into()),
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
        stale.inputs = vec![(header.clone().into(), "pkg/hdr.h".into())];
        assert!(!execute(&fixture.cache(), &stale).unwrap().cache_hit);
        // Other symlinks stay rejected as inputs.
        let outside = fixture.0.join("outside-link");
        symlink(&header, &outside).unwrap();
        consumer.inputs = vec![(outside.into(), "pkg/hdr.h".into())];
        assert!(
            execute(&fixture.cache(), &consumer)
                .unwrap_err()
                .contains("symlink is not allowed")
        );
    }

    /// A process's File executable that is a generated alias of a
    /// repository tree's file (an actions.symlink output: rules_python's
    /// interpreter executable) is staged as a symlink to that file, so a
    /// program resolving its own path (CPython's prefix search; here
    /// `readlink -f "$0"`) finds its installation in the tree; so is the
    /// file at its own execution path (`external/<repo>/...`) and as a
    /// runfile, every input of a repository tree. Writing through the link
    /// fails the action like any modified input, and the tree's file is
    /// never removed as a damaged blob.
    #[test]
    fn generated_executables_aliasing_tree_files_resolve_into_the_tree() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.cache()).unwrap();
        let root = fs::canonicalize(fixture.cache()).unwrap();
        let tree = root.join("repositories/direct/trees/abc");
        fs::create_dir_all(tree.join("lib")).unwrap();
        fs::create_dir_all(tree.join("bin")).unwrap();
        fs::write(tree.join("lib/landmark"), b"in the tree\n").unwrap();
        let tool = tree.join("bin/tool");
        let writer = tree.join("bin/writer");
        write_executable_script(
            &tool,
            "#!/bin/sh\nhome=$(dirname \"$(dirname \"$(readlink -f \"$0\")\")\")\ncat \"$home/lib/landmark\" > out/result\n",
            0o755,
        );
        write_executable_script(
            &writer,
            "#!/bin/sh\nprintf x >> \"$0\"\ntouch out/result\n",
            0o755,
        );
        let alias = |source: &Path, name: &str| alias_output(&fixture, source, name);
        let run = |id: &str, source: PathBuf, program: &str| {
            run_executable(&fixture, id, source, program)
        };
        let ran = run("alias", alias(&tool, "tool"), "bazel-out/bin/tools/tool").unwrap();
        assert_eq!(fs::read(&ran.outputs[0]).unwrap(), b"in the tree\n");
        for (id, program) in [
            ("direct", "external/repo/bin/tool"),
            ("runfile", "bazel-out/bin/x.runfiles/repo/bin/tool"),
        ] {
            let ran = run(id, tool.clone(), program).unwrap();
            assert_eq!(fs::read(&ran.outputs[0]).unwrap(), b"in the tree\n", "{id}");
        }
        // As a plain input too, not only as the process's executable.
        let mut action = fixture.action();
        action.id = "input".into();
        action.command = "sh external/repo/bin/tool".into();
        action.inputs = vec![(tool.clone().into(), "external/repo/bin/tool".into())];
        let ran = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(fs::read(&ran.outputs[0]).unwrap(), b"in the tree\n");
        let wrote = run(
            "writer",
            alias(&writer, "writer"),
            "bazel-out/bin/tools/writer",
        )
        .unwrap_err();
        assert!(
            wrote.contains(
                "input dependency bazel-out/bin/tools/writer was modified during execution."
            ),
            "{wrote}"
        );
        assert!(writer.is_file());
    }

    /// The published output of an actions.symlink action
    /// `bazel-out/bin/tools/<name>` aliasing `source` (its input at
    /// `external/repo/bin/<name>`), as rules_python's interpreter executable
    /// aliases the interpreter.
    fn alias_output(fixture: &Fixture, source: &Path, name: &str) -> PathBuf {
        let mut link = fixture.action();
        link.id = format!("alias-{name}");
        link.command.clear();
        link.inputs = vec![(
            source.to_path_buf().into(),
            format!("external/repo/bin/{name}").into(),
        )];
        link.outputs = vec![format!("bazel-out/bin/tools/{name}").into()];
        link.extra.transform = Some(Transform::Symlink {
            target: format!("external/repo/bin/{name}").into(),
            require_executable: true,
            progress: "Creating symlink".into(),
        });
        execute(&fixture.cache(), &link).unwrap().outputs[0].clone()
    }

    /// Run `source`, staged at the execution path `program`, as the
    /// process's File executable.
    fn run_executable(
        fixture: &Fixture,
        id: &str,
        source: PathBuf,
        program: &str,
    ) -> Result<ActionResult, String> {
        let mut action = fixture.action();
        action.id = id.into();
        // Keyed apart (the id is not keyed, SURVEY3 S1).
        action.extra.mnemonic = id.into();
        action.command.clear();
        action.program = Some(program.into());
        action.executable_file = true;
        action.inputs = vec![(source.into(), program.into())];
        execute(&fixture.cache(), &action)
    }

    /// Rewrite `path` in place with `contents` (from offset 0, no
    /// truncation: the same inode), without this process holding a
    /// writable descriptor to it (see write_executable_script: ETXTBSY).
    fn rewrite_in_place(path: &Path, contents: &str) {
        let status = Command::new("/bin/sh")
            .args(["-c", r#"printf '%s' "$2" 1<>"$1""#, "rewrite-in-place"])
            .arg(path)
            .arg(contents)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "cannot rewrite {}", path.display());
    }

    /// The file memo keeps a repository tree file's digest by its path for
    /// the whole process, so a tree file rewritten in place after a first
    /// action read it — same inode, size and mode, even its modification
    /// time restored — still has its old digest in the next action's key.
    /// Blob staging then stages the blob of those keyed bytes; a link into
    /// the tree (stage_tree_link) would run the new ones and publish their
    /// result under the old key, so the link is verified by content when it
    /// is staged.
    #[test]
    fn tree_links_verify_the_keyed_content_when_staged() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.cache()).unwrap();
        let root = fs::canonicalize(fixture.cache()).unwrap();
        let tool = root.join("repositories/direct/trees/abc/bin/tool");
        fs::create_dir_all(tool.parent().unwrap()).unwrap();
        write_executable_script(&tool, "#!/bin/sh\necho AAAA > out/result\n", 0o755);
        // Settled, so that the first read also enters the per-process digest
        // memo (by the birth-time key of a cache-owned file).
        let settled = SystemTime::now() - Duration::from_secs(60);
        File::open(&tool).unwrap().set_modified(settled).unwrap();
        let alias = alias_output(&fixture, &tool, "tool");
        let first = run_executable(&fixture, "first", alias.clone(), "bazel-out/bin/tools/tool");
        assert_eq!(fs::read(&first.unwrap().outputs[0]).unwrap(), b"AAAA\n");
        rewrite_in_place(&tool, "#!/bin/sh\necho BBBB > out/result\n");
        File::open(&tool).unwrap().set_modified(settled).unwrap();
        let error = run_executable(&fixture, "second", alias, "bazel-out/bin/tools/tool")
            .map(|second| fs::read_to_string(&second.outputs[0]).unwrap())
            .unwrap_err();
        assert_eq!(
            error,
            format!(
                "input '{}' changed while it was being staged",
                tool.display()
            )
        );
    }

    /// After the process, a link into a repository tree is judged by the
    /// tree's file (the link itself is the process's own): another inode
    /// renamed over that path, or its removal, fails the action like an
    /// in-place write, unless the path holds a file with the keyed content
    /// and mode (a republication of the same tree identity replaces every
    /// inode by identical bytes and modes).
    #[test]
    fn tree_links_count_replaced_tree_files_as_modified_unless_identical() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.cache()).unwrap();
        let root = fs::canonicalize(fixture.cache()).unwrap();
        let bin = root.join("repositories/direct/trees/abc/bin");
        fs::create_dir_all(&bin).unwrap();
        // Each script finds its tree file through the link.
        for (name, body) in [
            // Different bytes renamed over it.
            (
                "renamer",
                r#"cp "$t" "$t.new" && printf x >> "$t.new" && mv "$t.new" "$t""#,
            ),
            // The same bytes, another mode.
            (
                "moder",
                r#"cp "$t" "$t.new" && chmod 644 "$t.new" && mv "$t.new" "$t""#,
            ),
            // The same bytes and mode, as another inode.
            ("copier", r#"cp -p "$t" "$t.new" && mv "$t.new" "$t""#),
            ("remover", r#"rm "$t""#),
        ] {
            write_executable_script(
                &bin.join(name),
                format!("#!/bin/sh\nt=$(readlink -f \"$0\")\n{body} && touch out/result\n"),
                0o755,
            );
        }
        let run = |name: &str| {
            let alias = alias_output(&fixture, &bin.join(name), name);
            run_executable(
                &fixture,
                name,
                alias,
                &format!("bazel-out/bin/tools/{name}"),
            )
        };
        let modified = |name: &str| {
            format!(
                "action '{name}': input dependency bazel-out/bin/tools/{name} was modified during execution."
            )
        };
        assert_eq!(run("renamer").map(|_| ()), Err(modified("renamer")));
        assert_eq!(fs::read(bin.join("renamer")).unwrap().last(), Some(&b'x'));
        assert_eq!(run("moder").map(|_| ()), Err(modified("moder")));
        run("copier").unwrap();
        assert_eq!(run("remover").map(|_| ()), Err(modified("remover")));
    }

    /// Only an alias of a repository tree's file runs through a link into
    /// the store: an alias of a workspace source, of a published regular
    /// output or of a file in a repository rule's working area keeps blob
    /// staging, a regular file that resolves to itself in the execution
    /// root.
    #[test]
    fn aliases_of_sources_and_outputs_keep_blob_staging() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.cache()).unwrap();
        let root = fs::canonicalize(fixture.cache()).unwrap();
        let probe = "#!/bin/sh\nif [ -L \"$0\" ]; then echo link; else echo file; fi > out/result\nreadlink -f \"$0\" >> out/result\n";
        let source = fixture.0.join("workspace/bin/tool");
        let stage = root.join("repositories/direct/stages/slug/bin/tool");
        for path in [&source, &stage] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            write_executable_script(path, probe, 0o755);
        }
        let mut generate = fixture.write_action(probe.as_bytes().to_vec());
        generate.outputs = vec!["bazel-out/bin/gen/tool".into()];
        generate.write.as_mut().unwrap().executable = true;
        let output = execute(&fixture.cache(), &generate).unwrap().outputs[0].clone();
        assert!(fs::symlink_metadata(&output).unwrap().is_file());
        for (name, target) in [("source", source), ("output", output), ("stage", stage)] {
            let alias = alias_output(&fixture, &target, name);
            assert_eq!(fs::read_link(&alias).unwrap(), target, "{name}");
            let program = format!("bazel-out/bin/tools/{name}");
            let ran = run_executable(&fixture, name, alias, &program).unwrap();
            let result = fs::read_to_string(&ran.outputs[0]).unwrap();
            let (kind, resolved) = result.split_once('\n').unwrap();
            assert_eq!(kind, "file", "{name}");
            assert!(
                resolved.ends_with(&format!("/execroot/{program}\n")),
                "{name}: {resolved}"
            );
        }
    }

    /// A read-only file of a repository tree (rules_python's interpreter
    /// `lib/` after `chmod -R ugo-w`: 0444, the blob's own mode) is staged
    /// as a link to it (stage_tree_link) and never hardlinked into the
    /// store (ensure_blob copies such a file): the direct repository store
    /// rejects a tree file with a second link, so a linked blob made the
    /// interpreter's repository re-execute on the next command and republish
    /// over its read-only tree. A published output is still linked.
    #[test]
    fn read_only_repository_files_are_copied_into_the_store_never_linked() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.cache()).unwrap();
        let root = fs::canonicalize(fixture.cache()).unwrap();
        let module = root.join("repositories/direct/trees/abc/lib/python3.12/os.py");
        fs::create_dir_all(module.parent().unwrap()).unwrap();
        fs::write(&module, b"# stdlib\n").unwrap();
        fs::set_permissions(&module, fs::Permissions::from_mode(0o444)).unwrap();
        let mut action = fixture.action();
        action.inputs = vec![(module.clone().into(), "src/input".into())];
        let executed = execute(&fixture.cache(), &action).unwrap();
        assert_eq!(fs::read(&executed.outputs[0]).unwrap(), b"# stdlib\n");
        assert_eq!(fs::metadata(&module).unwrap().nlink(), 1);
        // The published output (0555, cache-owned) shares its blob's inode.
        assert!(fs::metadata(&executed.outputs[0]).unwrap().nlink() >= 2);
    }

    /// A published symlink output whose target lies in a repository tree
    /// store (an external repository's header or directory, as
    /// `_virtual_includes` links have): consumers read the target through
    /// the link, its store file once per process. A target in a repository
    /// rule's working area is read and checked on every use.
    #[test]
    fn symlink_outputs_into_store_trees_are_followed_and_memoized() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.cache()).unwrap();
        let root = fs::canonicalize(fixture.cache()).unwrap();
        let create = |path: &Path, text: &str| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
        };
        let tree = root.join("repositories/git/trees/abc");
        let header = tree.join("pkg/hdr.h");
        create(&header, "store\n");
        create(&tree.join("dir/a"), "a");
        create(&tree.join("dir/sub/b"), "b");
        let symlink_to = |id: &str, source: &Path, target: &str| {
            let mut link = fixture.action();
            link.id = id.into();
            link.command.clear();
            link.inputs = vec![(source.to_path_buf().into(), target.into())];
            link.outputs = vec![format!("out/{id}").into()];
            link.extra.transform = Some(Transform::Symlink {
                target: target.into(),
                require_executable: false,
                progress: "Creating symlink".into(),
            });
            link
        };
        let file_link = symlink_to("file-link", &header, "pkg/hdr.h");
        let linked = execute(&fixture.cache(), &file_link).unwrap();
        assert_eq!(fs::read_link(&linked.outputs[0]).unwrap(), header);
        let mut tree_link = symlink_to("tree-link", &tree.join("dir"), "in/dir");
        tree_link.extra.tree_inputs = BTreeSet::from([PathBuf::from("in/dir")]);
        tree_link.extra.tree_outputs = BTreeSet::from([PathBuf::from("out/tree-link")]);
        let tree_linked = execute(&fixture.cache(), &tree_link).unwrap();
        assert_eq!(
            fs::read_link(&tree_linked.outputs[0]).unwrap(),
            tree.join("dir")
        );
        assert_eq!(
            tree_linked.trees[&0],
            [PathBuf::from("a"), PathBuf::from("sub/b")]
        );
        let mut consumer = fixture.action();
        consumer.id = "consumer".into();
        consumer.inputs = vec![
            (linked.outputs[0].clone().into(), "x/hdr.h".into()),
            (tree_linked.outputs[0].clone().into(), "x/dir".into()),
        ];
        consumer.extra.tree_inputs = BTreeSet::from([PathBuf::from("x/dir")]);
        consumer.command = "cat x/hdr.h x/dir/a x/dir/sub/b > out/result".into();
        let consumed = execute(&fixture.cache(), &consumer).unwrap();
        assert_eq!(fs::read(&consumed.outputs[0]).unwrap(), b"store\nab");
        // The link is a published output, its target a store file, which
        // this process already read: following the link reads nothing.
        assert!(!store_file(Some(&root), &linked.outputs[0]));
        assert!(store_file(Some(&root), &header));
        let reads = FILE_READS.with(std::cell::Cell::get);
        let input = read_input(&linked.outputs[0], Some(&root), true).unwrap();
        assert_eq!((input.path.as_path(), input.size), (header.as_path(), 6));
        assert_eq!(FILE_READS.with(std::cell::Cell::get), reads);
        assert!(execute(&fixture.cache(), &file_link).unwrap().cache_hit);
        assert!(execute(&fixture.cache(), &consumer).unwrap().cache_hit);
        // The same link into a repository rule's working area: its target
        // is read again on every use, through the full ancestor walk.
        let stage = root.join("repositories/direct/stages/slug/pkg/hdr.h");
        create(&stage, "stage\n");
        let staged = execute(
            &fixture.cache(),
            &symlink_to("stage-link", &stage, "pkg/hdr.h"),
        )
        .unwrap();
        assert_eq!(fs::read_link(&staged.outputs[0]).unwrap(), stage);
        for _ in 0..2 {
            let (reads, walks) = (FILE_READS.with(std::cell::Cell::get), full_ancestor_walks());
            let input = read_input(&staged.outputs[0], Some(&root), true).unwrap();
            assert_eq!(input.path, stage);
            assert_eq!(FILE_READS.with(std::cell::Cell::get), reads + 1);
            assert_eq!(full_ancestor_walks(), walks + 1);
        }
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
        link.inputs = vec![(tree.outputs[0].clone().into(), "in/tree".into())];
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
        consumer.inputs = vec![(linked.outputs[0].clone().into(), "t".into())];
        consumer.extra.tree_inputs = BTreeSet::from([PathBuf::from("t")]);
        consumer.command = "cat t/a t/sub/b > out/result; stat -c %i t/a >> out/result".into();
        let consumed = execute(&fixture.cache(), &consumer).unwrap();
        assert_eq!(
            fs::read_to_string(&consumed.outputs[0]).unwrap(),
            format!("samesame{}\n", ino(&store))
        );
    }

    /// The per-process digest memo remembers a state it read only when the
    /// state was settled — by its modification and change time, for an
    /// owned file by its modification and birth time (an old modification
    /// time alone settles nothing) — and then for every path of the inode;
    /// what this process produced it remembers by that path alone, until it
    /// removes or replaces the file there.
    #[test]
    fn digest_memo_remembers_settled_states_and_produced_paths() {
        let fixture = Fixture::new();
        let path = fixture.0.join("fresh");
        fs::write(&path, b"fresh").unwrap();
        let (value, metadata) = hash_file(&path, false, false).unwrap();
        assert_eq!(value, digest(b"fresh"));
        // Just written: a same-size rewrite could keep the timestamps.
        assert_eq!(memoized(&path, &file_key(&metadata, false)), None);
        // An output being published is never remembered by its path.
        let (_, metadata) = hash_file(&path, true, true).unwrap();
        assert_eq!(memoized(&path, &file_key(&metadata, true)), None);
        let old = SystemTime::now() - Duration::from_secs(60);
        let later = SystemTime::now() + crate::digest_memo::RACY_MARGIN + Duration::from_secs(1);
        assert!(!settled(&fs::metadata(&path).unwrap(), old, false));
        assert!(settled(&fs::metadata(&path).unwrap(), later, false));
        assert!(settled(&fs::metadata(&path).unwrap(), later, true));
        // An old mtime with a fresh ctime and birth time (a `touch -d`, an
        // extracted archive, `cp -p`): racy for every file.
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let metadata = fs::metadata(&path).unwrap();
        assert!(!settled(&metadata, SystemTime::now(), true));
        assert!(!settled(&metadata, SystemTime::now(), false));
        let (_, metadata) = hash_file(&path, true, false).unwrap();
        assert_eq!(memoized(&path, &file_key(&metadata, true)), None);
        // A settled file (this crate's manifest): remembered for every path
        // of its inode, by its kind of key only.
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let (value, metadata) = hash_file(&manifest, false, false).unwrap();
        let key = file_key(&metadata, false);
        assert_eq!(memoized(&manifest, &key), Some((value, true)));
        assert_eq!(
            memoized(&fixture.0.join("a-link"), &key),
            Some((value, true))
        );
        assert_eq!(memoized(&manifest, &file_key(&metadata, true)), None);
        // Produced: by its path only, until it is forgotten.
        let blob = fixture.0.join("blob");
        fs::write(&blob, b"blob").unwrap();
        let metadata = fs::metadata(&blob).unwrap();
        let key = file_key(&metadata, true);
        remember_owned(&blob, &metadata, digest(b"blob"), None);
        assert_eq!(memoized(&blob, &key), Some((digest(b"blob"), false)));
        assert_eq!(memoized(&fixture.0.join("blob-link"), &key), None);
        forget_produced(&blob);
        assert_eq!(memoized(&blob, &key), None);
        // A published output below its action entry, until the entry goes.
        let entry = fixture.0.join("entry");
        let output = entry.join("outputs/out");
        fs::create_dir_all(output.parent().unwrap()).unwrap();
        fs::hard_link(&blob, &output).unwrap();
        remember_owned(&output, &metadata, digest(b"blob"), Some(&entry));
        assert_eq!(memoized(&output, &key), Some((digest(b"blob"), false)));
        forget_published(&entry);
        assert_eq!(memoized(&output, &key), None);
    }

    /// A file's digest is never served for another file: a file that is
    /// read, then deleted and replaced (at another path) by a file of the
    /// same size inside the same timestamp tick usually gets the deleted
    /// file's inode number and identical timestamps, birth time included
    /// (always on this overlayfs), so a memo keyed by the inode's stat alone
    /// answers for it. Covered: an execution-root output read for
    /// publication, a cache-owned file with an old (`touch -d`)
    /// modification time, and the replacement read as a file outside the
    /// cache (change-time key) after a cache-owned (birth-time key) read.
    /// Before the fix: about 1,800 of 1,800 inode numbers reused and
    /// thousands of stale digests.
    #[test]
    fn a_new_file_never_inherits_a_deleted_files_digest() {
        let fixture = Fixture::new();
        let old = SystemTime::now() - Duration::from_secs(60);
        // (owned, publishing)
        let reads: [(bool, bool); 3] = [(true, true), (true, false), (false, false)];
        let (mut reused, mut stale) = (0, Vec::new());
        for round in 0..300 {
            for (variant, &(owned, publishing)) in reads.iter().enumerate() {
                for old_mtime in [false, true] {
                    let write = |path: &Path, bytes: &[u8]| {
                        write_file(path, bytes, 0).unwrap();
                        if old_mtime {
                            File::options()
                                .write(true)
                                .open(path)
                                .unwrap()
                                .set_modified(old)
                                .unwrap();
                        }
                        set_mode(path, OUTPUT_MODE).unwrap();
                    };
                    let name = format!("{round}-{variant}-{old_mtime}");
                    let deleted = fixture.0.join(format!("deleted-{name}"));
                    write(&deleted, b"dupdupdu");
                    // (The previous round's files were deleted as well.)
                    if read_regular(&deleted, owned, publishing).unwrap().digest
                        != digest(b"dupdupdu")
                    {
                        stale.push(format!("{name}: first read"));
                    }
                    let inode = ino(&deleted);
                    fs::remove_file(&deleted).unwrap();
                    let new = fixture.0.join(format!("new-{name}"));
                    write(&new, b"u0000001");
                    reused += usize::from(ino(&new) == inode);
                    for (owned, publishing) in reads {
                        let found = read_regular(&new, owned, publishing).unwrap().digest;
                        if found != digest(b"u0000001") {
                            stale.push(format!(
                                "{name}: read as owned={owned} publishing={publishing}"
                            ));
                        }
                    }
                    fs::remove_file(&new).unwrap();
                }
            }
        }
        eprintln!("inode numbers reused: {reused} of {}", 300 * 3 * 2);
        assert!(
            stale.is_empty(),
            "{} stale digests (inode numbers reused {reused} times), first: {:?}",
            stale.len(),
            &stale[..stale.len().min(5)]
        );
    }

    /// The executor's path: write actions run one after another, outputs of
    /// one size alternating between a content whose blob already exists
    /// (its execution-root file is not linked into the store and goes away
    /// with the execution root) and a new one. Every published output has
    /// its own action's bytes, and a second process (fresh memos) finds
    /// every entry valid.
    #[test]
    fn outputs_of_one_size_are_published_with_their_own_content() {
        let fixture = Fixture::new();
        let content = |index: usize| {
            if index.is_multiple_of(2) {
                b"dupdupdu".to_vec()
            } else {
                format!("u{index:07}").into_bytes()
            }
        };
        let mut wrong = Vec::new();
        for index in 0..600 {
            let mut action = fixture.write_action(content(index));
            action.id = format!("write-{index}");
            action.outputs = vec![format!("out/f{index:05}").into()];
            let result = execute(&fixture.cache(), &action).unwrap();
            let published = fs::read(&result.outputs[0]).unwrap();
            if published != content(index) {
                wrong.push((index, String::from_utf8_lossy(&published).into_owned()));
            }
        }
        assert!(
            wrong.is_empty(),
            "{} outputs published with another action's bytes, first: {:?}",
            wrong.len(),
            &wrong[..wrong.len().min(5)]
        );
    }

    /// No digest of a file this process did not observe reaches the
    /// persistent memo: a file outside the cache (a source being saved)
    /// that reuses the inode number of an execution-root output read for
    /// publication and removed with its execution root, in the same
    /// timestamp tick, is read for its own bytes and — racy — not recorded.
    /// Before the fix, the stale per-process answer was recorded (trusted)
    /// for the new file and the next process read it from the table (1,862
    /// of 2,000 such files in a scratch run against 0363050).
    #[test]
    fn stale_states_never_reach_the_persistent_table() {
        use crate::digest_memo::{Key, Table};
        let fixture = Fixture::new();
        let cache = fixture.cache();
        let work = cache.join("work/x");
        let outside = fixture.0.join("outside");
        fs::create_dir_all(&work).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let table: &'static Table = Box::leak(Box::new(Table::new(&cache)));
        TEST_TABLE.with(|cell| cell.set(Some(table)));
        let mut wrong = Vec::new();
        for round in 0..500 {
            let deleted = work.join(format!("a{round}"));
            write_file(&deleted, b"dupdupdu", 0).unwrap();
            set_mode(&deleted, OUTPUT_MODE).unwrap();
            read_regular(&deleted, true, true).unwrap();
            fs::remove_file(&deleted).unwrap();
            let new = outside.join(format!("b{round}"));
            write_file(&new, b"u0000001", 0).unwrap();
            if read_regular(&new, false, false).unwrap().digest != digest(b"u0000001") {
                wrong.push(round);
            }
        }
        // A settled file read and hashed: a record that makes the table
        // worth saving (trusted records only ride along). No other test of
        // this process reads it.
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.lock");
        read_regular(&manifest, false, false).unwrap();
        TEST_TABLE.with(|cell| cell.set(None));
        assert!(matches!(
            table.save(),
            Ok(crate::digest_memo::Saved::Written { .. })
        ));
        let reloaded = Table::new(&cache);
        let recorded = (0..500)
            .filter(|round| {
                let path = outside.join(format!("b{round}"));
                let key = Key::of(&fs::symlink_metadata(&path).unwrap(), false).unwrap();
                reloaded.lookup(&path, &key).is_some()
            })
            .count();
        assert!(wrong.is_empty(), "stale digests in rounds {wrong:?}");
        assert_eq!(recorded, 0, "racy files were recorded");
        let key = Key::of(&fs::symlink_metadata(&manifest).unwrap(), false).unwrap();
        assert!(reloaded.lookup(&manifest, &key).is_some());
    }

    /// A process an action leaves behind in its group (here a background
    /// `sleep`) is killed once the action's process exits, like Bazel's
    /// process-wrapper does: it could otherwise still change the outputs
    /// after they were hashed and published.
    #[test]
    fn processes_an_action_leaves_behind_are_killed() {
        let alive = |pid: &str| {
            fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    let (_, rest) = stat.rsplit_once(')')?;
                    rest.trim_start().chars().next()
                })
                .is_some_and(|state| !matches!(state, 'Z' | 'X'))
        };
        let fixture = Fixture::new();
        let mut action = fixture.action();
        action.command = "sleep 60 & echo $! > out/result".into();
        let result = execute(&fixture.cache(), &action).unwrap();
        let pid = fs::read_to_string(&result.outputs[0]).unwrap();
        let pid = pid.trim();
        assert!(!pid.is_empty());
        // The SIGKILLed process still has to run its exit; on a loaded host
        // that can take a while. 20 s is far below the 60 s it would sleep.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let killed = loop {
            if !alive(pid) {
                break true;
            }
            if std::time::Instant::now() > deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        if !killed {
            let _ = Command::new("kill").args(["-9", pid]).status();
        }
        assert!(killed, "background process {pid} outlived its action");
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
