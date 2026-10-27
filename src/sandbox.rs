//! Sandboxed local execution (N59, docs/SANDBOX.md).
//!
//! Bazel 9.3's `sandboxed` strategy on Linux is the `linux-sandbox` tool
//! (src/main/tools/linux-sandbox.cc): clone(CLONE_NEWUSER | CLONE_NEWNS |
//! CLONE_NEWIPC | CLONE_NEWPID [| CLONE_NEWNET | CLONE_NEWUTS]) and, in pid 1,
//! a private read-only mount table where only the sandbox execroot, TMPDIR,
//! /tmp and /dev/shm stay writable (linux-sandbox-pid1.cc:
//! MakeFilesystemMostlyReadOnly). This host runs the build client inside a
//! Docker container whose default seccomp policy denies unshare/clone with
//! CLONE_NEWNS or CLONE_NEWPID and every mount(2) (probe receipt in
//! tests/fixtures/sandbox/reference_receipt.json), so those namespaces cannot
//! be created by an unprivileged build client here.
//!
//! The closest Bazel-faithful design that still makes out-of-sandbox writes
//! FAIL is a Landlock ruleset (LSM since Linux 5.13; ABI 4 here) applied in a
//! sandbox worker process before exec'ing the action, exactly where
//! linux-sandbox would apply its namespace setup:
//!
//! - every write-ish filesystem access is denied outside the paths Bazel's
//!   linux-sandbox keeps writable (LinuxSandboxedSpawnRunner.getWritableDirs
//!   adds the sandbox execroot, /tmp and /dev/shm;
//!   AbstractSandboxSpawnRunner.getWritableDirs adds TMPDIR, TEST_TMPDIR and
//!   --sandbox_writable_path entries with resolved symlinks);
//! - reads stay allowed everywhere, like a read-only bind of the whole
//!   filesystem; only --sandbox_block_path subtrees lose read access;
//! - the network stays allowed by default, exactly like Bazel
//!   (--sandbox_default_allow_network, default true): TCP bind and connect
//!   are denied only when the spawn must be denied the network — the
//!   `block-network` tag, or --nosandbox_default_allow_network without a
//!   `requires-network` tag (Spawns.requiresNetwork; the Landlock
//!   counterpart of the network namespace linux-sandbox creates only then) —
//!   approximated to TCP because Landlock (ABI 4) has no UDP or raw-socket
//!   rules;
//! - denials surface as EACCES ("Permission denied"), not the EROFS a
//!   read-only remount produces — the observable difference from genuine
//!   linux-sandbox, documented in the receipt.
//!
//! No user, PID, IPC, UTS or mount namespace exists, and Landlock cannot
//! restrict process visibility, so `/proc` shows host processes. Those
//! namespaces are simply unavailable to unprivileged processes under this
//! container's seccomp filter; the deviations are recorded in docs/SANDBOX.md.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use landlock::{
    AccessFs, AccessNet, BitFlags, Compatible, PathBeneath, PathFd, PathFdError, Ruleset,
    RulesetAttr, RulesetCreated, RulesetCreatedAttr, RulesetError,
};

/// AbstractSandboxSpawnRunner.SANDBOX_DEBUG_SUGGESTION (verbatim, including
/// the two leading newlines that follow describeCommandFailure's output).
pub const SANDBOX_DEBUG_SUGGESTION: &str = "\n\nUse --sandbox_debug to see verbose messages from the sandbox and retain the sandbox build root for debugging";

/// The hidden argv[1] that dispatches main to [`worker_main`].
pub const WORKER_ARG: &str = "__sandbox-worker";

/// The strategies this engine registers, in the identifier spellings Bazel
/// 9.3 registers them: "standalone" and "local" (StandaloneModule.java:98),
/// "sandboxed" and "linux-sandbox" (SandboxModule.java:326),
/// "processwrapper-sandbox" (SandboxModule.java:290-299 — a sandbox that
/// isolates nothing; the strategy genuine Bazel resolves to on hosts where
/// namespaces are unavailable, like this one) and "worker"
/// (BazelStrategyModule.java:75; WorkerSpawnRunner, crate::worker: usable for
/// spawns that support persistent workers).
const IDENTIFIER_STANDALONE: &str = "standalone";
const IDENTIFIER_SANDBOXED: &str = "sandboxed";

/// Every registered identifier, in the order Bazel's
/// SpawnStrategyRegistry.toStrategy error lists them.
const REGISTERED: &[&str] = &[
    "standalone",
    "local",
    "sandboxed",
    "linux-sandbox",
    "processwrapper-sandbox",
    "worker",
];

/// How many actions executed under each strategy, for the build summary.
static EXECUTED_SANDBOXED: AtomicU64 = AtomicU64::new(0);
static EXECUTED_STANDALONE: AtomicU64 = AtomicU64::new(0);

/// The execution strategy one action runs under.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Strategy {
    Standalone,
    /// The Landlock sandbox (this engine's `sandboxed`/`linux-sandbox`).
    Sandboxed,
    /// `processwrapper-sandbox`: a sandbox strategy that isolates nothing —
    /// the spawn runs in its private execroot exactly like standalone, but
    /// counts as sandboxed (SandboxModule.java:285-299). Escape hatch
    /// matching genuine Bazel on hosts without namespaces.
    ProcessWrapper,
    /// `worker`: persistent workers (crate::worker); usable only for spawns
    /// that support them (WorkerSpawnRunner.canExec), skipped otherwise like
    /// SpawnStrategyRegistry filtering on canExec.
    Worker,
    /// `remote` (RemoteSpawnStrategy, registered by RemoteModule with a
    /// remote cache or executor): usable for a spawn that may be executed
    /// remotely (RemoteExecutionService.mayBeExecutedRemotely: an executor
    /// and no `local`/`no-remote`/`no-remote-exec` requirement;
    /// docs/REMOTE_EXECUTION.md).
    Remote,
}

impl Strategy {
    /// The registered command-line identifier (SpawnStrategy.getName).
    pub fn identifier(self) -> &'static str {
        match self {
            Strategy::Standalone => IDENTIFIER_STANDALONE,
            Strategy::Sandboxed => IDENTIFIER_SANDBOXED,
            Strategy::ProcessWrapper => "processwrapper-sandbox",
            Strategy::Worker => "worker",
            Strategy::Remote => "remote",
        }
    }

    /// The runner name of a spawn under this strategy
    /// (SpawnRunner.getName, SpawnStats): Landlock is this engine's
    /// `linux-sandbox`, standalone is LocalSpawnRunner's `local`.
    pub fn runner_name(self) -> &'static str {
        match self {
            Strategy::Standalone => "local",
            Strategy::Sandboxed => "linux-sandbox",
            Strategy::ProcessWrapper => "processwrapper-sandbox",
            Strategy::Worker => "worker",
            Strategy::Remote => "remote",
        }
    }

    /// Whether this strategy runs the spawn inside a sandbox of any strength
    /// (the build summary's "sandboxed" note).
    fn is_sandbox(self) -> bool {
        matches!(self, Strategy::Sandboxed | Strategy::ProcessWrapper)
    }
}

/// The spawn and sandbox options: ExecutionOptions's spawnStrategy and
/// strategy entries and the SandboxOptions subset this engine models.
pub struct SpawnOptions {
    /// `--spawn_strategy`: comma-separated, highest priority first
    /// (CommaSeparatedNonEmptyOptionListConverter). Not allowMultiple: a
    /// repeated flag replaces the list (ExecutionOptions.java:63-75).
    pub spawn_strategy: Vec<String>,
    /// `--strategy=<mnemonic>=<list>` in flag order; the last entry for a
    /// mnemonic wins (a multimap put,
    /// SpawnStrategyRegistry.java:359-361 with BazelStrategyModule.java:84-89).
    pub strategy: Vec<(String, Vec<String>)>,
    /// `--genrule_strategy`: the Genrule-only override Bazel 9.3 deprecates in
    /// favor of `--strategy=Genrule=` (ExecutionOptions.java:77-87). A
    /// repeated flag replaces the list.
    pub genrule_strategy: Vec<String>,
    /// `--sandbox_writable_path` (SandboxOptions.java:161-170).
    pub sandbox_writable_path: Vec<PathBuf>,
    /// `--sandbox_block_path` (SandboxOptions.java:141-147).
    pub sandbox_block_path: Vec<PathBuf>,
    /// `--sandbox_tmpfs_path` (SandboxOptions.java:149-159).
    pub sandbox_tmpfs_path: Vec<PathBuf>,
    /// `--sandbox_add_mount_pair` identity mounts (SandboxOptions.java:
    /// 173-180; [`mount_pair`]). Recorded only: every host path is already
    /// visible at its own path in the Landlock sandbox and a bind mount does
    /// not make its target writable (linux-sandbox remounts everything
    /// outside the writable dirs read-only), so an identity mount changes
    /// nothing a spawn observes here. Non-identity remounts are rejected at
    /// option parsing (docs/SANDBOX.md).
    pub sandbox_add_mount_pair: Vec<PathBuf>,
    /// `--sandbox_debug` (SandboxOptions.java:87-97).
    pub sandbox_debug: bool,
    /// `--sandbox_default_allow_network`, default true (SandboxOptions.java:
    /// 283-292): sandboxed actions reach the network unless the spawn tags
    /// `block-network` or the flag is off (and the spawn does not tag
    /// `requires-network`).
    pub sandbox_default_allow_network: bool,
    /// A remote cache or executor is configured: RemoteModule registers the
    /// `remote` strategy (RemoteModule.registerSpawnStrategies).
    pub remote_registered: bool,
    /// `--remote_executor` is set: `remote` heads the default strategies
    /// (BazelStrategyModule.java:67-80) and can execute spawns.
    pub remote_executor: bool,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            spawn_strategy: Vec::new(),
            strategy: Vec::new(),
            genrule_strategy: Vec::new(),
            sandbox_writable_path: Vec::new(),
            sandbox_block_path: Vec::new(),
            sandbox_tmpfs_path: Vec::new(),
            sandbox_add_mount_pair: Vec::new(),
            sandbox_debug: false,
            sandbox_default_allow_network: true,
            remote_registered: false,
            remote_executor: false,
        }
    }
}

static OPTIONS: OnceLock<SpawnOptions> = OnceLock::new();

/// Installs the invocation's options (once, from main).
pub fn install(options: SpawnOptions) {
    let _ = OPTIONS.set(options);
}

fn options() -> &'static SpawnOptions {
    OPTIONS.get_or_init(SpawnOptions::default)
}

/// `--sandbox_debug` (it changes failure messages and retention, so the
/// executor reads it directly).
pub fn debug() -> bool {
    options().sandbox_debug
}

/// SandboxOptions.MountPairConverter (:50-80): `source[:target]`, split on
/// every `:` not preceded by `\` with Java's String.split (trailing empty
/// pieces dropped), `\:` unescaped; a blank piece or more than two pieces
/// fail with the converter's text. Returns `(source, target)`.
pub fn mount_pair(input: &str) -> Result<(String, String), String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut previous = None;
    for character in input.chars() {
        if character == ':' && previous != Some('\\') {
            pieces.push(std::mem::take(&mut current));
        } else {
            current.push(character);
        }
        previous = Some(character);
    }
    pieces.push(current);
    // String.split(regex) with limit 0 drops trailing empty strings, except
    // that an input without any match yields itself.
    if pieces.len() > 1 {
        while pieces.last().is_some_and(String::is_empty) {
            pieces.pop();
        }
    }
    const FORM: &str = "Input must be a single path to mount inside the sandbox or a mounting pair in the form of 'source:target'";
    let mut paths = Vec::new();
    for piece in pieces {
        // String.trim strips every char <= U+0020.
        if piece.trim_matches(|c: char| c <= ' ').is_empty() {
            return Err(format!(
                "Input {input} contains one or more empty paths. {FORM}"
            ));
        }
        paths.push(piece.replace("\\:", ":"));
    }
    match <[String; 1]>::try_from(paths) {
        Ok([path]) => Ok((path.clone(), path)),
        Err(paths) => match <[String; 2]>::try_from(paths) {
            Ok([source, target]) => Ok((source, target)),
            Err(_) => Err(FORM.to_owned()),
        },
    }
}

/// Rejects unknown strategy identifiers once at startup with Bazel's
/// SpawnStrategyRegistry.toStrategy text (:717-727): the request name is
/// "explicit default strategies" for --spawn_strategy (built at :546-551)
/// and "mnemonic M" for --strategy=M= / --genrule_strategy (the Genrule
/// mnemonic filter of BazelStrategyModule.java:84, built at :494). The
/// trailing "Valid values are: [...]" is approximate: genuine's set is the
/// strategies its modules registered on the host (an order the registry
/// does not define); this engine's fixed set follows.
pub fn validate(options: &SpawnOptions) -> Result<(), String> {
    let registered = registered(options);
    let check = |identifier: &str, request: &str| -> Result<(), String> {
        if !registered.contains(&identifier) {
            return Err(format!(
                "'{identifier}' was requested for {request} but no strategy with that \
                 identifier was registered. Valid values are: [{}]",
                registered.join(", ")
            ));
        }
        Ok(())
    };
    for identifier in &options.spawn_strategy {
        check(identifier, "explicit default strategies")?;
    }
    for identifier in &options.genrule_strategy {
        check(identifier, "mnemonic Genrule")?;
    }
    for (mnemonic, list) in &options.strategy {
        for identifier in list {
            check(identifier, &format!("mnemonic {mnemonic}"))?;
        }
    }
    Ok(())
}

/// The registered identifiers: [`REGISTERED`], plus `remote` when a remote
/// cache or executor is configured.
fn registered(options: &SpawnOptions) -> Vec<&'static str> {
    let mut identifiers = REGISTERED.to_vec();
    if options.remote_registered {
        identifiers.push("remote");
    }
    identifiers
}

/// Whether `--remote_executor` is set (the `remote` strategy can execute).
pub fn remote_executor() -> bool {
    options().remote_executor
}

/// The Landlock ABI the ruleset requests; every handled access is a
/// HardRequirement, so a kernel lacking any of them (TRUNCATE is ABI 3, TCP
/// rules ABI 4) fails the setup loudly instead of silently enforcing
/// nothing.
const ABI: landlock::ABI = landlock::ABI::V3;

/// Whether this kernel's Landlock can carry the sandbox: a ruleset handling
/// the write accesses must be creatable. Probed once per process; mirrors
/// LinuxSandboxedSpawnRunner.isSupported, which runs /bin/true through the
/// linux-sandbox once and remembers the result. The library's unit tests
/// execute actions in-process, where the worker (a hidden subcommand of the
/// CLI binary) cannot run, so they resolve to the next strategy exactly like
/// a host whose kernel lacks Landlock; sandboxed execution itself is covered
/// by the CLI integration tests (tests/sandbox.rs).
pub fn supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    if cfg!(test) {
        return false;
    }
    *SUPPORTED.get_or_init(|| {
        Ruleset::default()
            .set_compatibility(landlock::CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_write(ABI))
            .and_then(|ruleset| ruleset.create().map(|_| ()))
            .is_ok()
    })
}

/// The strategies requested for `mnemonic`, in priority order
/// (SpawnStrategyRegistry.getStrategies:129-146 — the per-mnemonic `--strategy`
/// list beats `--spawn_strategy`, its last entry for the mnemonic winning
/// (BazelStrategyModule.java:84-89 puts into the multimap);
/// `--strategy_regexp` is not modelled). An empty request selects the
/// engine's platform default: `worker`, `sandboxed`, then `standalone`
/// (BazelStrategyModule.java:67-80: [remote,] worker, sandboxed, local).
fn strategies(mnemonic: &str) -> Result<Vec<Strategy>, String> {
    let options = options();
    let requested: &[String] = if let Some((_, list)) = options
        .strategy
        .iter()
        .rev()
        .find(|(name, _)| name == mnemonic)
    {
        list
    } else if mnemonic == "Genrule" && !options.genrule_strategy.is_empty() {
        &options.genrule_strategy
    } else {
        &options.spawn_strategy
    };
    let identifiers: Vec<&str> = if requested.is_empty() {
        if options.remote_executor {
            vec![
                "remote",
                "worker",
                IDENTIFIER_SANDBOXED,
                IDENTIFIER_STANDALONE,
            ]
        } else {
            vec!["worker", IDENTIFIER_SANDBOXED, IDENTIFIER_STANDALONE]
        }
    } else {
        requested
            .iter()
            .map(String::as_str)
            .filter(|identifier| !identifier.is_empty())
            .collect()
    };
    identifiers
        .iter()
        .map(|identifier| match *identifier {
            IDENTIFIER_STANDALONE | "local" => Ok(Strategy::Standalone),
            IDENTIFIER_SANDBOXED | "linux-sandbox" => Ok(Strategy::Sandboxed),
            "processwrapper-sandbox" => Ok(Strategy::ProcessWrapper),
            "worker" => Ok(Strategy::Worker),
            "remote" if options.remote_registered => Ok(Strategy::Remote),
            other => Err(format!(
                "'{other}' was requested for --spawn_strategy but no strategy with that \
                 identifier was registered. Valid values are: [{}]",
                registered(options).join(", ")
            )),
        })
        .collect()
}

/// Resolves the strategy for one action: the highest-priority strategy that
/// can execute it (SpawnStrategyResolver.resolveOne plus
/// AbstractSpawnStrategy.canExec / Spawns.mayBeSandboxed, Spawns.java:63-67:
/// actions tagged `no-sandbox`, `nosandbox` or `local` cannot run sandboxed
/// and fall through to the next strategy of the list; `worker` executes only
/// spawns that support persistent workers, `worker` here being
/// crate::worker::can_exec's verdict). `execution_info` merges the rule tags
/// (Action.execution_info) with the spawn's execution_requirements map
/// (ActionExtra.execution_info), like remote_policy's merge
/// (crate::action::remote_policy). Fails with Bazel's message when no
/// requested strategy can execute the spawn (SpawnStrategyResolver.java:
/// 77-96; the strategy list prints as a Java list).
pub fn resolve(
    mnemonic: &str,
    execution_info: &BTreeSet<String>,
    worker: bool,
) -> Result<Strategy, String> {
    let list = strategies(mnemonic)?;
    let sandboxable = !execution_info.contains("no-sandbox")
        && !execution_info.contains("nosandbox")
        && !execution_info.contains("local");
    // Spawns.mayBeExecutedRemotely (actions/Spawns.java:52-56).
    let remotable = options().remote_executor
        && !execution_info.contains("local")
        && !execution_info.contains("no-remote")
        && !execution_info.contains("no-remote-exec");
    for strategy in &list {
        match strategy {
            Strategy::Standalone | Strategy::ProcessWrapper => return Ok(*strategy),
            Strategy::Sandboxed if sandboxable && supported() => return Ok(*strategy),
            Strategy::Worker if worker => return Ok(*strategy),
            Strategy::Remote if remotable => return Ok(*strategy),
            Strategy::Sandboxed | Strategy::Worker | Strategy::Remote => continue,
        }
    }
    Err(format!(
        "{mnemonic} spawn cannot be executed with any of the available strategies: [{}]. Your \
         --spawn_strategy, --genrule_strategy, --strategy and/or \
         --allowed_strategies_by_exec_platform flags are probably too strict. Visit \
         https://github.com/bazelbuild/bazel/issues/7480 for advice.",
        list.iter()
            .map(|strategy| strategy.identifier())
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// Whether the sandbox must deny network access to the spawn
/// (LinuxSandboxedSpawnRunner.java:302-303: the network namespace is created
/// when the spawn does not require network; Spawns.requiresNetwork, :78-88:
/// `block-network` -> false, `requires-network` -> true, else the
/// --sandbox_default_allow_network flag, default true; the java_debug
/// override of SandboxHelpers.shouldAllowNetwork is not modelled).
pub fn denies_network(execution_info: &BTreeSet<String>) -> bool {
    if execution_info.contains("block-network") {
        return true;
    }
    if execution_info.contains("requires-network") {
        return false;
    }
    !options().sandbox_default_allow_network
}

/// Records that one action ran under `strategy` (the build summary names the
/// execution mode; Bazel has no such line).
pub fn count(strategy: Strategy) {
    if strategy.is_sandbox() {
        EXECUTED_SANDBOXED.fetch_add(1, Ordering::Relaxed);
    } else {
        EXECUTED_STANDALONE.fetch_add(1, Ordering::Relaxed);
    }
}

/// Whether every executed action ran sandboxed (meaningful after execution;
/// the summary prints once the build ends).
pub fn all_sandboxed() -> bool {
    EXECUTED_STANDALONE.load(Ordering::Relaxed) == 0
        && EXECUTED_SANDBOXED.load(Ordering::Relaxed) > 0
}

/// The sandbox worker's configuration, passed as the hidden `__sandbox-worker`
/// subcommand's arguments (the counterpart of linux-sandbox's -M/-w/-e/-N
/// command line, LinuxSandboxCommandLineBuilder).
struct WorkerConfig {
    /// Writable paths: the action's execroot and TMPDIR, TEST_TMPDIR, /tmp and
    /// /dev/shm (LinuxSandboxedSpawnRunner.getWritableDirs:391-398), plus
    /// --sandbox_writable_path and --sandbox_tmpfs_path entries.
    writable: Vec<PathBuf>,
    /// --sandbox_block_path entries.
    blocked: Vec<PathBuf>,
    /// Whether the sandbox must deny network access to the spawn
    /// (crate::denies_network: the Landlock counterpart of creating the
    /// network namespace, LinuxSandboxedSpawnRunner.java:302-303 — created
    /// only when the spawn does not require network; by default Bazel allows
    /// network access in sandboxes, --sandbox_default_allow_network default
    /// true, SandboxOptions.java:283-292).
    deny_network: bool,
    /// Where the worker's setup log goes when --sandbox_debug is on
    /// (linux-sandbox's -d debug_path).
    debug_path: Option<PathBuf>,
    /// Where the worker reports a sandbox setup failure (the executor reads it
    /// after the wait; no action exit code can be mistaken for one).
    setup_error: PathBuf,
    /// The process argv[0] spelling when it differs from the executable (test
    /// spawns exec the setup script with the test binary as argv[0],
    /// StandaloneTestStrategy's test wrapper).
    arg0: Option<String>,
    /// Exec the command in place of the sandbox worker instead of
    /// supervising it: a sandboxed persistent worker (crate::worker), whose
    /// stdin and stdout are the worker protocol's streams.
    exec: bool,
}

impl WorkerConfig {
    /// The worker's argv for `arguments`: `[__sandbox-worker, ...config..., --,
    /// argv...]`. The worker runs with the action's own cwd and environment;
    /// only the configuration travels as arguments.
    fn argv(&self, arguments: &[String]) -> Vec<String> {
        let mut argv = vec![WORKER_ARG.to_owned()];
        for path in &self.writable {
            argv.push("--writable".into());
            argv.push(display(path));
        }
        for path in &self.blocked {
            argv.push("--block".into());
            argv.push(display(path));
        }
        if self.deny_network {
            argv.push("--deny-network".into());
        }
        if let Some(path) = &self.debug_path {
            argv.push("--debug".into());
            argv.push(display(path));
        }
        argv.push("--setup-error".into());
        argv.push(display(&self.setup_error));
        if let Some(arg0) = &self.arg0 {
            argv.push("--arg0".into());
            argv.push(arg0.clone());
        }
        if self.exec {
            argv.push("--exec".into());
        }
        argv.push("--".into());
        argv.extend(arguments.iter().cloned());
        argv
    }
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_default()
            .as_path()
            .join(path)
    }
}

/// Wraps one action command line in the sandbox worker: the executor starts
/// its own binary as `__sandbox-worker` (the role the linux-sandbox binary
/// fills as the spawn's argv[0]; crate::process_wrapper::Launch::sandbox_worker)
/// and the worker runs and supervises the action under the Landlock ruleset.
/// The writable set is the paths Bazel's linux-sandbox keeps
/// writable for the spawn; the caller has already created the execroot and
/// tmp directories and resolved the action environment (TMPDIR/TEST_TMPDIR
/// are read from the worker's own environment, which is the action's).
pub fn wrap(
    execroot: &Path,
    tmp: &Path,
    environment: &BTreeMap<String, std::ffi::OsString>,
    deny_network: bool,
    arguments: &[String],
    debug_root: &Path,
    arg0: Option<&str>,
) -> Vec<String> {
    let options = options();
    let mut writable = vec![
        execroot.to_path_buf(),
        tmp.to_path_buf(),
        PathBuf::from("/tmp"),
        // The linux-sandbox mounts a private, writable /dev with the standard
        // device nodes (linux-sandbox-pid1.cc MountDev:645-672); the closest
        // Landlock equivalent is granting the host's /dev writable (the node
        // set is the host's, not a minimal tmpfs — docs/SANDBOX.md).
        PathBuf::from("/dev"),
    ];
    // /dev/shm stays writable like the linux-sandbox's
    // (LinuxSandboxedSpawnRunner.getWritableDirs:395); hosts without it skip
    // the grant (genuine Bazel's resolveSymbolicLinks would fail the setup).
    if Path::new("/dev/shm").exists() {
        writable.push(PathBuf::from("/dev/shm"));
    }
    // AbstractSandboxSpawnRunner.getWritableDirs:352-405: the spawn
    // environment's TEST_TMPDIR stays writable, resolved when it exists
    // (addWritablePath: a path below the sandbox execroot is added as-is;
    // TMPDIR is `tmp`, already first in the list).
    if let Some(value) = environment.get("TEST_TMPDIR")
        && let Some(value) = value.to_str()
        && !value.is_empty()
    {
        let mut path = PathBuf::from(value);
        if let Ok(resolved) = fs::canonicalize(&path) {
            path = resolved;
        }
        if !writable.contains(&path) {
            writable.push(path);
        }
    }
    for path in &options.sandbox_writable_path {
        // The path and its symlink target both stay writable
        // (AbstractSandboxSpawnRunner.getWritableDirs:393-402).
        writable.push(absolute(path));
        if let Ok(resolved) = fs::canonicalize(path)
            && !writable.contains(&resolved)
        {
            writable.push(resolved);
        }
    }
    for path in &options.sandbox_tmpfs_path {
        writable.push(absolute(path));
    }
    WorkerConfig {
        writable,
        blocked: options
            .sandbox_block_path
            .iter()
            .map(|path| absolute(path))
            .collect(),
        deny_network,
        debug_path: options
            .sandbox_debug
            .then(|| debug_root.join("sandbox-debug.log")),
        setup_error: debug_root.join("sandbox-setup-error"),
        arg0: arg0.map(str::to_owned),
        exec: false,
    }
    .argv(arguments)
}

/// [`wrap`] for a sandboxed persistent worker (`--worker_sandboxing`,
/// SandboxedWorker): the same writable set around the worker's working
/// directory, the network allowed (genuine's sandboxed workers isolate the
/// filesystem only), and the worker process exec'ed in place of the sandbox
/// worker so that its stdin and stdout stay the protocol's streams. A setup
/// failure is reported on stderr (the worker log) with exit code 126, before
/// any response.
pub fn wrap_worker(
    execroot: &Path,
    tmp: &Path,
    environment: &BTreeMap<String, std::ffi::OsString>,
    arguments: &[String],
    debug_root: &Path,
) -> Vec<String> {
    let mut argv = wrap(
        execroot,
        tmp,
        environment,
        false,
        arguments,
        debug_root,
        None,
    );
    let separator = argv
        .iter()
        .position(|argument| argument == "--")
        .expect("wrap ends its options with --");
    argv.insert(separator, "--exec".into());
    argv
}

/// The worker entry point (main dispatches `__sandbox-worker` here): apply the
/// Landlock ruleset, then start and supervise the action
/// (crate::process_wrapper). A sandbox setup failure — including an action
/// that cannot be started — is written to the setup-error file and reported
/// with exit code 126 before the action exists (the infrastructure-failure
/// channel; the action's own status travels in the control channel's end
/// report) — the counterpart of AbstractSandboxSpawnRunner.runSpawn's
/// sandbox.createFileSystem failure ("Could not copy inputs into sandbox", an
/// EnvironmentalExecException, not the action's).
pub fn worker_main(args: &[String]) -> i32 {
    match worker(args) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("linux-sandbox: {error}");
            // The executor reports the message from the file, so the action's
            // own stderr stays untouched by the infrastructure failure.
            for (index, argument) in args.iter().enumerate() {
                if argument == "--setup-error"
                    && let Some(path) = args.get(index + 1)
                {
                    let _ = fs::write(path, format!("linux-sandbox: {error}\n"));
                }
            }
            126
        }
    }
}

fn worker(args: &[String]) -> Result<i32, String> {
    let mut config = WorkerConfig {
        writable: Vec::new(),
        blocked: Vec::new(),
        deny_network: false,
        debug_path: None,
        setup_error: PathBuf::new(),
        arg0: None,
        exec: false,
    };
    let mut arguments: &[String] = &[];
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--writable" => {
                index += 1;
                config.writable.push(path(&args.get(index))?);
            }
            "--block" => {
                index += 1;
                config.blocked.push(path(&args.get(index))?);
            }
            "--deny-network" => config.deny_network = true,
            "--exec" => config.exec = true,
            "--debug" => {
                index += 1;
                config.debug_path = Some(path(&args.get(index))?);
            }
            "--setup-error" => {
                index += 1;
                config.setup_error = path(&args.get(index))?;
            }
            "--arg0" => {
                index += 1;
                config.arg0 = Some(
                    args.get(index)
                        .ok_or_else(|| "sandbox worker: --arg0 requires a value".to_owned())?
                        .clone(),
                );
            }
            "--" => {
                arguments = &args[index + 1..];
                break;
            }
            other => return Err(format!("sandbox worker: unknown option '{other}'")),
        }
        index += 1;
    }
    if arguments.is_empty() {
        return Err("sandbox worker: no command after --".into());
    }
    let debug = |line: &str| {
        if let Some(path) = &config.debug_path {
            use std::io::Write;
            if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(file, "{line}");
            }
        }
    };
    debug(&format!(
        "linux-sandbox (landlock) worker: writable={:?} blocked={:?} deny_network={}",
        config.writable, config.blocked, config.deny_network
    ));
    restrict(&config, &debug)?;
    // The worker stays the action's parent and supervises it exactly like the
    // process-wrapper (crate::process_wrapper): a child subreaper that, once
    // the action has exited, kills every process it left — also those that
    // left its process group — the way linux-sandbox's PID namespace ends
    // with its pid 1 (linux-sandbox-pid1.cc:811-814). The action inherits the
    // ruleset, the worker's cwd and environment (the executor set them); the
    // argv[0] spelling is the executor's, whose arguments these are. The
    // action's exact wait status reaches the executor through the control
    // channel.
    let mut command = Command::new(&arguments[0]);
    command.args(&arguments[1..]);
    if let Some(arg0) = &config.arg0 {
        use std::os::unix::process::CommandExt;
        command.arg0(arg0);
    }
    if config.exec {
        // A persistent worker: the worker process replaces this one under
        // the ruleset (Landlock domains survive exec), keeping its stdin and
        // stdout, and the executor's worker pool manages its lifetime.
        let error = crate::spawn_lock::exec(&mut command);
        return Err(format!("cannot execute '{}': {error}", arguments[0]));
    }
    crate::process_wrapper::run_supervised(&mut command)
        .map_err(|error| format!("cannot execute '{}': {error}", arguments[0]))
}

fn path(argument: &Option<&String>) -> Result<PathBuf, String> {
    argument
        .map(PathBuf::from)
        .ok_or_else(|| "sandbox worker: option requires a value".to_owned())
}

/// Formats one Landlock ruleset error with the sandbox role it failed in.
fn ruleset_error(error: RulesetError) -> String {
    format!("cannot set up the sandbox: {error}")
}

/// Applies the sandbox: a Landlock ruleset handling every write access
/// (denying it everywhere the writable rules do not grant it) and TCP bind and
/// connect (unless the spawn requires network). Reads are handled only when
/// paths are blocked, denied on the blocked subtrees; otherwise reads are not
/// handled at all, so everything stays readable like Bazel's read-only mounts.
/// The typed signal that a sandbox setup failure occurred: the CLI's exit
/// code 36 comes from this flag, never from matching the error text (an
/// action's own stdout/stderr could contain the phrase). Set exactly where
/// the executor reads the worker's setup-error report.
static SETUP_FAILURE: AtomicBool = AtomicBool::new(false);

/// Records a sandbox setup failure (an environmental failure of the
/// strategy, Bazel's EnvironmentalExecException class -> exit code 36).
pub fn note_setup_failure() {
    SETUP_FAILURE.store(true, Ordering::Relaxed);
}

/// Whether any sandbox setup failure occurred during this invocation.
pub fn setup_failure_occurred() -> bool {
    SETUP_FAILURE.load(Ordering::Relaxed)
}

fn restrict(config: &WorkerConfig, debug: &dyn Fn(&str)) -> Result<(), String> {
    // Every handled access is a HardRequirement: a kernel lacking any of
    // them (the write set includes ABI 3's TRUNCATE; the network rules are
    // ABI 4) fails the setup loudly — the sandbox never degrades into a
    // silently unenforced ruleset.
    let ruleset = Ruleset::default().set_compatibility(landlock::CompatLevel::HardRequirement);
    let ruleset = ruleset
        .handle_access(AccessFs::from_write(ABI))
        .map_err(ruleset_error)?;
    let ruleset = if config.deny_network {
        ruleset
            .handle_access(AccessNet::BindTcp)
            .and_then(|ruleset| ruleset.handle_access(AccessNet::ConnectTcp))
            .map_err(ruleset_error)?
    } else {
        ruleset
    };
    let ruleset = if config.blocked.is_empty() {
        ruleset
    } else {
        ruleset
            .handle_access(AccessFs::ReadFile)
            .and_then(|ruleset| ruleset.handle_access(AccessFs::ReadDir))
            .map_err(ruleset_error)?
    };
    let mut created = ruleset.create().map_err(ruleset_error)?;
    let write = AccessFs::from_write(ABI);
    for path in &config.writable {
        debug(&format!("writable: {}", path.display()));
        let (next, added) = add_path_rule(created, path, write, "writable")?;
        if !added {
            return Err(format!("writable path '{}' does not exist", path.display()));
        }
        created = next;
    }
    if !config.blocked.is_empty() {
        // Landlock rules only grant access; there is no deny rule. Blocking a
        // path therefore means granting read on every other top-level subtree
        // (and, along each blocked path, on each sibling), the same total walk
        // MakeFilesystemMostlyReadOnly does over /proc/self/mounts. Execution
        // stays unhandled everywhere: like the read-only mounts, binaries
        // outside the blocked subtrees keep running.
        for granted in read_grants(&config.blocked)? {
            debug(&format!("readable: {}", granted.display()));
            // LANDLOCK_ACCESS_FS_READ_DIR is directory-only: a file sibling
            // grants only the read-file right.
            let access: BitFlags<AccessFs> =
                if fs::symlink_metadata(&granted).is_ok_and(|metadata| metadata.is_dir()) {
                    AccessFs::ReadFile | AccessFs::ReadDir
                } else {
                    AccessFs::ReadFile.into()
                };
            let (next, _) = add_path_rule(created, &granted, access, "readable")?;
            created = next;
        }
    }
    let status = created.restrict_self().map_err(ruleset_error)?;
    // The enforcement probe: with the HardRequirement compat level a
    // partially or entirely unenforced filesystem ruleset must never pass
    // silently — treat it as a setup error (RestrictionStatus.ruleset,
    // landlock::RulesetStatus).
    if status.ruleset != landlock::RulesetStatus::FullyEnforced {
        return Err(format!(
            "the sandbox was not enforced on this kernel ({:?}); refusing to run the action \
             unsandboxed",
            status.ruleset
        ));
    }
    debug("landlock: restricted and fully enforced");
    Ok(())
}

/// Grants `access` on `path`'s subtree, or marks the grant skipped when a
/// readable target cannot be opened with an error a read-only bind would not
/// report either (MakeFilesystemMostlyReadOnly ignores EACCES, EPERM, EINVAL,
/// ENOENT, ESTALE and ENODEV; a dangling symlink among a directory's entries
/// is the ENOENT case here). A missing writable path still fails, like
/// linux-sandbox's mount(2) would.
fn add_path_rule(
    created: RulesetCreated,
    path: &Path,
    access: BitFlags<AccessFs>,
    role: &str,
) -> Result<(RulesetCreated, bool), String> {
    let fd = match PathFd::new(path) {
        Ok(fd) => fd,
        Err(PathFdError::OpenCall { source, .. })
            if role == "readable"
                && matches!(
                    source.kind(),
                    std::io::ErrorKind::NotFound
                        | std::io::ErrorKind::PermissionDenied
                        | std::io::ErrorKind::NotADirectory
                ) =>
        {
            return Ok((created, false));
        }
        Err(error) => {
            return Err(format!(
                "cannot open {role} path '{}': {error}",
                path.display()
            ));
        }
    };
    created
        .add_rule(PathBeneath::new(fd, access))
        .map(|created| (created, true))
        .map_err(|error| format!("cannot add {role} rule '{}': {error}", path.display()))
}

/// The subtrees that stay readable when `blocked` paths must become
/// inaccessible: every top-level path except the blocked ones' first segment,
/// plus — along each blocked path — the siblings of every component, so
/// exactly the blocked subtrees (and nothing else) lose read access.
fn read_grants(blocked: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut grants = Vec::new();
    let roots: Vec<PathBuf> = fs::read_dir("/")
        .map_err(|error| format!("cannot enumerate / for --sandbox_block_path: {error}"))?
        .filter_map(|entry| entry.ok())
        .map(|entry| PathBuf::from("/").join(entry.file_name()))
        .collect();
    'root: for root in roots {
        for path in blocked {
            if first_segment(path).is_some_and(|first| first == root) {
                // Below a blocked path's first segment: grant the siblings
                // along the way instead of the whole subtree.
                grants.extend(sibling_grants(&root, path)?);
                continue 'root;
            }
        }
        grants.push(root);
    }
    Ok(grants)
}

/// The first segment of an absolute `path` (its top-level directory).
fn first_segment(path: &Path) -> Option<PathBuf> {
    path.components().nth(1).map(|component| {
        let mut first = PathBuf::from("/");
        first.push(component.as_os_str());
        first
    })
}

/// The readable siblings along `blocked` below `root`: for every component of
/// `blocked` from `root` down to `blocked` itself, each sibling of that
/// component stays readable. `blocked` equal to `root` grants nothing below
/// it.
fn sibling_grants(root: &Path, blocked: &Path) -> Result<Vec<PathBuf>, String> {
    let rest = blocked
        .strip_prefix(root)
        .map_err(|_| format!("blocked path '{}' is not below its root", blocked.display()))?;
    let mut current = root.to_path_buf();
    let mut grants = Vec::new();
    for component in rest.components() {
        // current/component is the blocked path at this depth; its siblings
        // stay readable.
        let target = current.join(component.as_os_str());
        let siblings = fs::read_dir(&current)
            .map_err(|error| format!("cannot enumerate '{}': {error}", current.display()))?;
        for sibling in siblings.filter_map(|entry| entry.ok()) {
            let path = current.join(sibling.file_name());
            if path != target {
                grants.push(path);
            }
        }
        current = target;
    }
    Ok(grants)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    /// MountPairConverter texts captured from genuine 9.3.0
    /// (tests/fixtures/sandbox_mount_pair/reference_receipt.json) plus
    /// Java String.split edge cases.
    #[test]
    fn mount_pair_follows_the_converter() {
        let pair = |a: &str, b: &str| Ok((a.to_owned(), b.to_owned()));
        assert_eq!(mount_pair("/tmp"), pair("/tmp", "/tmp"));
        assert_eq!(mount_pair("/tmp:/tmp"), pair("/tmp", "/tmp"));
        assert_eq!(mount_pair("/usr:/opt/mp"), pair("/usr", "/opt/mp"));
        // Trailing empty pieces are dropped by String.split.
        assert_eq!(mount_pair("/a:"), pair("/a", "/a"));
        assert_eq!(mount_pair("/a\\:b:/c"), pair("/a:b", "/c"));
        let form = "Input must be a single path to mount inside the sandbox or a mounting pair in the form of 'source:target'";
        assert_eq!(
            mount_pair("/a::/b"),
            Err(format!(
                "Input /a::/b contains one or more empty paths. {form}"
            ))
        );
        assert_eq!(
            mount_pair(" "),
            Err(format!("Input   contains one or more empty paths. {form}"))
        );
        assert_eq!(
            mount_pair(":/a"),
            Err(format!(
                "Input :/a contains one or more empty paths. {form}"
            ))
        );
        assert_eq!(mount_pair("/a:/b:/c"), Err(form.to_owned()));
        assert_eq!(mount_pair("::"), Err(form.to_owned()));
    }

    #[test]
    fn suggestion_is_bazels_verbatim_text() {
        // AbstractSandboxSpawnRunner.java:70-72.
        assert_eq!(
            SANDBOX_DEBUG_SUGGESTION,
            "\n\nUse --sandbox_debug to see verbose messages from the sandbox \
             and retain the sandbox build root for debugging"
        );
    }

    #[test]
    fn worker_argv_round_trips() {
        let config = WorkerConfig {
            writable: vec![PathBuf::from("/w")],
            blocked: vec![PathBuf::from("/b")],
            deny_network: true,
            debug_path: Some(PathBuf::from("/d")),
            setup_error: PathBuf::from("/e"),
            arg0: None,
            exec: false,
        };
        let argv = config.argv(&strings(&["/bin/true", "x"]));
        assert_eq!(
            argv,
            [
                "__sandbox-worker",
                "--writable",
                "/w",
                "--block",
                "/b",
                "--deny-network",
                "--debug",
                "/d",
                "--setup-error",
                "/e",
                "--",
                "/bin/true",
                "x"
            ]
        );
        // The worker parser accepts its own output.
        let mut parsed = WorkerConfig {
            writable: Vec::new(),
            blocked: Vec::new(),
            deny_network: false,
            debug_path: None,
            setup_error: PathBuf::new(),
            arg0: None,
            exec: false,
        };
        let mut arguments: &[String] = &[];
        let mut index = 1;
        while index < argv.len() {
            match argv[index].as_str() {
                "--writable" => {
                    index += 1;
                    parsed.writable.push(PathBuf::from(&argv[index]));
                }
                "--block" => {
                    index += 1;
                    parsed.blocked.push(PathBuf::from(&argv[index]));
                }
                "--deny-network" => parsed.deny_network = true,
                "--debug" => {
                    index += 1;
                    parsed.debug_path = Some(PathBuf::from(&argv[index]));
                }
                "--setup-error" => {
                    index += 1;
                    parsed.setup_error = PathBuf::from(&argv[index]);
                }
                "--" => {
                    arguments = &argv[index + 1..];
                    break;
                }
                _ => unreachable!("argv produced by argv()"),
            }
            index += 1;
        }
        assert_eq!(parsed.writable, config.writable);
        assert_eq!(parsed.blocked, config.blocked);
        assert!(parsed.deny_network);
        assert_eq!(parsed.debug_path, config.debug_path);
        assert_eq!(parsed.setup_error, config.setup_error);
        assert_eq!(arguments, strings(&["/bin/true", "x"]).as_slice());
    }

    #[test]
    fn first_segment_is_the_top_level_directory() {
        assert_eq!(
            first_segment(Path::new("/home/coder/x")),
            Some(PathBuf::from("/home"))
        );
        assert_eq!(
            first_segment(Path::new("/tmp")),
            Some(PathBuf::from("/tmp"))
        );
        assert_eq!(first_segment(Path::new("/")), None);
    }

    #[test]
    fn sibling_grants_cover_everything_but_the_blocked_branch() {
        let root = crate::test_temp::root().join(format!("n59-grants-{}", std::process::id()));
        fs::create_dir_all(root.join("blocked/inner")).unwrap();
        fs::create_dir_all(root.join("open")).unwrap();
        fs::write(root.join("loose.txt"), b"x").unwrap();
        fs::write(root.join("blocked/inner/secret.txt"), b"s").unwrap();
        fs::write(root.join("blocked/visible.txt"), b"v").unwrap();
        let grants = sibling_grants(&root, &root.join("blocked/inner")).unwrap();
        // Every sibling at each depth along the blocked path stays readable.
        assert!(grants.contains(&root.join("open")));
        assert!(grants.contains(&root.join("loose.txt")));
        assert!(grants.contains(&root.join("blocked/visible.txt")));
        // The blocked branch itself is never granted.
        assert!(!grants.contains(&root.join("blocked/inner")));
        assert!(
            !grants
                .iter()
                .any(|path| path.starts_with(root.join("blocked/inner")))
        );
        // Blocking the whole root grants nothing below it.
        assert!(sibling_grants(&root, &root).unwrap().is_empty());
        fs::remove_dir_all(&root).unwrap();
    }
}
