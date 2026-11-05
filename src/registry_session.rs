//! Per-invocation registry access: lockfile hashes, a content-addressed
//! registry file cache, a URL memo and bounded parallel prefetching.
//!
//! Semantics follow IndexRegistry.doGrabFile (IndexRegistry.java:166-243)
//! with the KnownFileHashesMode of RegistryFactoryImpl.java:59-71:
//! `off` ignores hashes, `update` uses them (and trusts recorded "not
//! found"), `refresh` re-checks "not found", `error` requires them. Files
//! fetched with a checksum (MODULE.bazel, source.json, bazel_registry.json)
//! are recorded like RegistryFileDownloadEvent (RegistryFileDownloadEvent.java)
//! and served from the repository cache by SHA-256 when known
//! (DownloadManager.downloadAndReadOneUrlForBzlmod, :414-497); metadata.json
//! is mutable and never hashed (IndexRegistry.java:586-592). Local (`file:`)
//! registries ignore hashes and are never recorded in the lockfile
//! (BazelLockFileModule.java:158-167).
use crate::lockfile::{self, Lockfile, LockfileMode};
use crate::registry::Registry;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Concurrent registry requests (Bazel's discovery fetches a whole horizon
/// in parallel through Skyframe, Discovery.java:113-137).
const PARALLEL_FETCHES: usize = 32;

/// The fewest registry files [`RegistrySession::warm_known_files`] fetches
/// ahead.
const WARM_MIN_FILES: usize = if cfg!(test) { 1 } else { 16 };

/// The result of one registry file request.
#[derive(Clone, Debug)]
pub enum Fetch {
    Found(Arc<Vec<u8>>),
    /// The NotFoundException message, e.g. `URL: not found`.
    NotFound(String),
}

/// Where lockfile updates go (native-only `--native-lockfile`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LockfileTarget {
    /// Bazel: read and write `<workspace>/MODULE.bazel.lock`.
    Workspace,
    /// Read the workspace lockfile, never write any lockfile.
    ReadOnly,
    /// A private copy: read it when it exists (else the workspace lockfile),
    /// write it instead of the workspace lockfile.
    Overlay(PathBuf),
}

impl LockfileTarget {
    /// `--native-lockfile=workspace|readonly|/absolute/path`.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "workspace" => Ok(Self::Workspace),
            "readonly" => Ok(Self::ReadOnly),
            path if Path::new(path).is_absolute() => Ok(Self::Overlay(PathBuf::from(path))),
            _ => Err(format!(
                "--native-lockfile expects workspace, readonly or an absolute path, got {value:?}"
            )),
        }
    }
}

/// Allowed yanked versions (YankedVersionsUtil.parseAllowedYankedVersions):
/// None allows all.
pub type AllowedYanked = Option<BTreeSet<String>>;

/// One URL's fetch, performed at most once per invocation.
type FetchSlot = OnceLock<Result<Fetch, String>>;

#[derive(Debug)]
pub struct RegistrySession {
    mode: LockfileMode,
    workspace: PathBuf,
    target: LockfileTarget,
    proof_store: Option<PathBuf>,
    lockfile: Lockfile,
    write_path: Option<PathBuf>,
    cas: Option<PathBuf>,
    allowed_yanked: AllowedYanked,
    memo: Mutex<HashMap<String, Arc<FetchSlot>>>,
    recorded: Mutex<BTreeMap<String, Option<String>>>,
    resolution: Mutex<Option<Resolution>>,
    /// Whether every module extension the command demanded resolved
    /// successfully in this invocation (the loading phase completed: each
    /// one evaluated, or reused from an up-to-date MODULE.bazel.lock
    /// entry). BazelLockFileModule.afterCommand combines
    /// BazelModuleResolutionValue and BazelDepGraphValue with the
    /// SingleExtensionValues "evaluated in the current build" (:95-100,
    /// :105-141). The native gate is fail-closed and STRICTER than Bazel:
    /// until the whole module resolution — demanded extensions included —
    /// succeeded, `updated_lockfile` and `updated_hidden_lockfile` stay
    /// None and nothing is written, whereas genuine Bazel still writes
    /// after a demanded extension failed (E1c2: rc=1, lockfile rewritten;
    /// expected native mismatch).
    extensions_evaluated: AtomicBool,
    /// The resolved count (evaluated or reused) of the gated invocation,
    /// for the write-path diagnostic only.
    resolved_extensions: AtomicUsize,
    /// Normalized registry URL -> spelling as given (IndexRegistry.getUrl()).
    spellings: BTreeMap<String, String>,
    /// The hidden lockfile (Bazel: `<output_base>/MODULE.bazel.lock`,
    /// BazelLockFileFunction.java:75-79), read in update mode; any read
    /// failure is the empty lockfile (:96-98). None without a cache path.
    hidden: Option<(PathBuf, Lockfile)>,
    /// The hidden lockfile was read from a seed (crate::worktree::Seed): it
    /// is written to its own path at the end even when nothing changed, so
    /// the next command reads it there.
    hidden_seeded: bool,
    /// How the hidden lockfile was read without being observed, for the
    /// analysis snapshot's record of it ([`Self::note_hidden_lockfile`]);
    /// `None` when its read was observed (a publication proof's session)
    /// or none happened.
    hidden_read: Option<HiddenRead>,
    /// The module extensions whose hidden lockfile entry or facts this
    /// invocation consulted (crate::repository's extension_lock), in every
    /// lockfile mode: what the snapshot's record of it covers, with
    /// `extension_results`.
    hidden_consulted: Mutex<std::collections::BTreeSet<String>>,
    /// The SingleExtensionValues done in this invocation, in completion order.
    extension_results: Mutex<Vec<crate::lockfile_extensions::ExtensionResult>>,
    /// The module extensions evaluated in this invocation whose result
    /// validated as the next command's reuse of its lockfile entry would
    /// ([`Self::note_settled_extension`]): each holds a provisional taint
    /// until [`Self::note_hidden_lockfile`] confirms the entry stands where
    /// the next command reads it.
    settled_extensions: Mutex<Vec<SettledExtension>>,
    /// `--native-lockfile=readonly`: a stateless probe mode that writes no
    /// lockfile at all, not even the hidden one (reads are kept).
    readonly: bool,
}

/// A module extension evaluated in this invocation whose result validated
/// as the next command's reuse of its lockfile entry would
/// ([`RegistrySession::note_settled_extension`]).
#[derive(Clone, Debug)]
pub(crate) struct SettledExtension {
    /// The provisional taint's detail (crate::observed_fs::provisional_taint).
    pub key: String,
    /// GsonTypeAdapterUtil's ModuleExtensionId spelling.
    pub id: String,
    pub factors: String,
    pub facts_version: i64,
    /// The entry as the next command parses it from the lockfile
    /// (crate::lockfile_extensions::reparsed_entry).
    pub entry: lockfile::Json,
}

/// What BazelModuleResolutionValue and the BazelDepGraphValue usage table
/// contribute to the written lockfile.
#[derive(Clone, Debug)]
pub struct Resolution {
    pub selected_yanked: Vec<(String, String)>,
    /// None when the usage table could not be computed (Bazel then has no
    /// BazelDepGraphValue and writes nothing, BazelLockFileModule.java:95-100).
    pub usages: Option<ExtensionUsages>,
}

/// Extension IDs with usages in the resolved graph.
#[derive(Clone, Debug, Default)]
pub struct ExtensionUsages {
    pub ids: BTreeSet<String>,
    pub isolated_prefixes: BTreeSet<String>,
}

impl ExtensionUsages {
    pub fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
            || self
                .isolated_prefixes
                .iter()
                .any(|prefix| id.starts_with(prefix.as_str()))
    }
}

/// The resolver's optional session; equality is identity.
#[derive(Clone, Debug, Default)]
pub struct SessionHandle(Option<Arc<RegistrySession>>);

impl PartialEq for SessionHandle {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}
impl Eq for SessionHandle {}

/// The lockfile's view of yanked information for one selected module
/// (IndexRegistry.tryGetYankedVersionsFromLockfile, :612-650).
#[derive(Debug, PartialEq, Eq)]
pub enum YankedLookup {
    /// Known from the lockfile: the yanked info of the selected version, if any.
    Known(Option<String>),
    /// metadata.json must be fetched.
    Fetch,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HashesMode {
    Ignore,
    UseAndUpdate,
    UseImmutableAndUpdate,
    Enforce,
}

impl RegistrySession {
    /// Read the lockfile for `mode` (BazelLockFileFunction.compute: `off` is
    /// the empty lockfile without reading) and create the session.
    pub fn open(
        workspace: &Path,
        mode: LockfileMode,
        target: &LockfileTarget,
        repository_cache: Option<PathBuf>,
        allowed_yanked: AllowedYanked,
    ) -> Result<Self, String> {
        let workspace_lockfile = workspace.join(lockfile::LOCKFILE_NAME);
        let (read_path, write_path) = match target {
            LockfileTarget::Workspace => (workspace_lockfile.clone(), Some(workspace_lockfile)),
            LockfileTarget::ReadOnly => (workspace_lockfile, None),
            LockfileTarget::Overlay(path) => (
                if crate::observed_fs::exists(path) {
                    path.clone()
                } else {
                    workspace_lockfile
                },
                Some(path.clone()),
            ),
        };
        let lockfile = if mode == LockfileMode::Off {
            Lockfile::default()
        } else {
            lockfile::read(&read_path, mode)?
        };
        Ok(Self {
            mode,
            workspace: workspace.to_path_buf(),
            target: target.clone(),
            proof_store: None,
            lockfile,
            write_path,
            cas: repository_cache
                .map(|cache| crate::materializer::download_cache_dir(&cache, "sha256")),
            allowed_yanked,
            memo: Mutex::default(),
            recorded: Mutex::default(),
            resolution: Mutex::default(),
            extensions_evaluated: AtomicBool::new(false),
            resolved_extensions: AtomicUsize::new(0),
            spellings: BTreeMap::new(),
            hidden: None,
            hidden_seeded: false,
            hidden_read: None,
            hidden_consulted: Mutex::default(),
            extension_results: Mutex::default(),
            settled_extensions: Mutex::default(),
            readonly: *target == LockfileTarget::ReadOnly,
        })
    }

    /// Use `path` as the hidden lockfile (native: under `--cache-dir`, keyed
    /// by the workspace, like Bazel's per-workspace output base).
    pub fn with_hidden_lockfile(self, path: PathBuf) -> Self {
        self.with_seeded_hidden_lockfile(path, &[])
    }

    /// [`Self::with_hidden_lockfile`]; while `path` does not exist, the
    /// first existing seed is read instead (docs/WORKTREES.md): the
    /// workspace's hidden lockfile of the flat cache layout, then a linked
    /// worktree's main checkout's. Every entry is validated before it is
    /// used, like any lockfile entry (`reusable_lock_entry`), and the entries
    /// of another workspace's lockfile that mention that workspace's path
    /// are dropped ([`without_mentions`]). A seeded lockfile is written to
    /// `path` when the command ends.
    pub fn with_seeded_hidden_lockfile(
        mut self,
        path: PathBuf,
        seeds: &[crate::worktree::Seed],
    ) -> Self {
        if self.mode == LockfileMode::Off {
            self.hidden = Some((path, Lockfile::default()));
            return self;
        }
        // Not observed: the analysis snapshot records the entries this
        // command used instead ([`Self::note_hidden_lockfile`]).
        crate::observed_fs::discard(|| self.read_hidden_lockfile(path, seeds, false))
    }

    /// [`Self::with_hidden_lockfile`] with the read observed like any file
    /// (a publication proof's private recording keeps it).
    fn with_observed_hidden_lockfile(mut self, path: PathBuf) -> Self {
        if self.mode == LockfileMode::Off {
            self.hidden = Some((path, Lockfile::default()));
            return self;
        }
        self.read_hidden_lockfile(path, &[], true)
    }

    fn read_hidden_lockfile(
        mut self,
        path: PathBuf,
        seeds: &[crate::worktree::Seed],
        observed: bool,
    ) -> Self {
        // The stat key before the read, as a READ records it.
        let read = |path: &Path| {
            let key = crate::observed_fs::stat_key(path);
            let bytes = crate::observed_fs::read(path);
            let digest = bytes.as_ref().ok().map(|bytes| {
                use sha2::Digest as _;
                <[u8; 32]>::from(sha2::Sha256::digest(bytes))
            });
            let lockfile = match bytes {
                Ok(bytes) => {
                    Lockfile::parse(&String::from_utf8_lossy(&bytes), LockfileMode::Update)
                        .unwrap_or_default()
                }
                Err(_) => Lockfile::default(),
            };
            (lockfile, key, digest)
        };
        if !crate::observed_fs::exists(&path)
            && let Some(seed) = seeds
                .iter()
                .find(|seed| crate::observed_fs::exists(&seed.path))
        {
            let (mut lockfile, key, digest) = read(&seed.path);
            if let Some(foreign) = &seed.foreign {
                lockfile = without_mentions(lockfile, foreign);
            }
            if !observed {
                self.hidden_read = Some(match digest {
                    Some(sha256) => HiddenRead::Seed {
                        path: seed.path.clone(),
                        key,
                        sha256,
                    },
                    None => HiddenRead::SeedUnreadable(seed.path.clone()),
                });
            }
            self.hidden = Some((path, lockfile));
            self.hidden_seeded = true;
            return self;
        }
        let (lockfile, key, _) = read(&path);
        if !observed {
            self.hidden_read = Some(HiddenRead::Own { key });
        }
        self.hidden = Some((path, lockfile));
        self
    }

    /// Module extension `id`'s hidden lockfile entry or facts were consulted.
    pub(crate) fn note_hidden_consulted(&self, id: &str) {
        self.hidden_consulted
            .lock()
            .expect("hidden lockfile consultations")
            .insert(id.to_owned());
    }

    /// Record the hidden lockfile for the analysis snapshot (instead of its
    /// bytes, which another command — a `query` evaluating other extensions
    /// — rewrites), once this command decided what it writes: by the entries
    /// of the module extensions it resolved or consulted
    /// ([`Lockfile::entries_digest`], crate::observed_fs::note_lockfile_entries)
    /// in the hidden lockfile as it stands once this command wrote it (its
    /// serialized form parsed again, exactly what a later reader parses;
    /// always parsed on validation), else as it read it (with the stat key
    /// of that read). A hidden lockfile read from a seed and not written is
    /// recorded as the seed's READ with its own path absent. A later
    /// command reads the same entries for the same extensions and resolves
    /// them alike. Every save path calls it; a repeat changes nothing.
    pub fn note_hidden_lockfile(&self) {
        if self.mode == LockfileMode::Off {
            return;
        }
        self.confirm_settled_extensions();
        let (Some((path, read)), Some(how)) = (&self.hidden, &self.hidden_read) else {
            return;
        };
        let mut ids = self
            .hidden_consulted
            .lock()
            .expect("hidden lockfile consultations")
            .clone();
        ids.extend(
            self.extension_results
                .lock()
                .expect("extension results")
                .iter()
                .map(|result| result.id.clone()),
        );
        match (self.updated_hidden_lockfile(), how) {
            (Some((_, written)), _) => {
                let reparsed =
                    Lockfile::parse(&written.serialize(), LockfileMode::Update).unwrap_or_default();
                let digest = reparsed.entries_digest(&ids);
                crate::observed_fs::note_lockfile_entries(path, ids, digest, None);
            }
            (None, HiddenRead::Own { key }) => {
                let digest = read.entries_digest(&ids);
                crate::observed_fs::note_lockfile_entries(path, ids, digest, Some(*key));
            }
            (
                None,
                HiddenRead::Seed {
                    path: seed,
                    key,
                    sha256,
                },
            ) => {
                crate::observed_fs::note_seeded_lockfile(path, seed, *key, *sha256);
            }
            (None, HiddenRead::SeedUnreadable(seed)) => {
                crate::observed_fs::taint_after_window(crate::observed_fs::Taint::Unstable, || {
                    format!(
                        "hidden lockfile seed {} could not be read",
                        crate::observed_fs::escape(seed.as_os_str())
                    )
                });
            }
        }
    }

    /// Whether this command writes the hidden lockfile when it succeeds
    /// (a lockfile mode that writes; not `--native-lockfile=readonly`).
    pub(crate) fn writes_hidden_lockfile(&self) -> bool {
        self.mode.writes() && !self.readonly && self.hidden.is_some()
    }

    /// Module extension `settled.id` was evaluated, and its result validated
    /// as the next command's reuse of its lockfile entry would (its recorded
    /// inputs up to date, its repositories and metadata those the entry
    /// gives, nothing printed; crate::repository): its provisional taint
    /// (crate::observed_fs::provisional_taint) is confirmed once the entry
    /// stands where the next command reads it ([`Self::note_hidden_lockfile`]).
    pub(crate) fn note_settled_extension(&self, settled: SettledExtension) {
        self.settled_extensions
            .lock()
            .expect("settled extensions")
            .push(settled);
    }

    /// Confirm the provisional taint of each settled extension whose entry
    /// the next command — reading both lockfiles as this command leaves them
    /// (serialized, parsed again) — selects (the workspace lockfile's
    /// entry first, then the hidden one's) and reuses as far as the
    /// lockfiles decide it (the facts check of
    /// SingleExtensionEvalFunction.tryGettingValueFromLockFile). Any other
    /// outcome keeps the taint: no write, a readonly or error mode, another
    /// entry, facts that differ.
    fn confirm_settled_extensions(&self) {
        let settled = self
            .settled_extensions
            .lock()
            .expect("settled extensions")
            .clone();
        if settled.is_empty() || !self.writes_hidden_lockfile() {
            return;
        }
        let reparse = |lockfile: &Lockfile, mode: LockfileMode| {
            Lockfile::parse(&lockfile.serialize(), mode).ok()
        };
        let workspace = match self.updated_lockfile() {
            Some(written) => reparse(&written, self.mode),
            None => Some(self.lockfile.clone()),
        };
        let hidden = match self.updated_hidden_lockfile() {
            Some((_, written)) => reparse(&written, LockfileMode::Update),
            None => Some(self.hidden_lockfile().clone()),
        };
        let (Some(workspace), Some(hidden)) = (workspace, hidden) else {
            return;
        };
        for extension in settled {
            let selection = crate::lockfile_extensions::lock_selection(
                &workspace,
                &hidden,
                &extension.id,
                &extension.factors,
                extension.facts_version,
            );
            if selection.locked.as_ref() == Some(&extension.entry)
                && !crate::lockfile_extensions::facts_block_reuse(
                    &extension.entry,
                    &selection.facts,
                    selection.hidden_facts.as_deref(),
                )
            {
                crate::observed_fs::confirm_provisional(&extension.key);
            }
        }
    }

    /// The hidden lockfile's value (empty without one).
    pub fn hidden_lockfile(&self) -> &Lockfile {
        static EMPTY: OnceLock<Lockfile> = OnceLock::new();
        self.hidden.as_ref().map_or_else(
            || EMPTY.get_or_init(Lockfile::default),
            |(_, lockfile)| lockfile,
        )
    }

    /// A SingleExtensionValue was computed (evaluated or taken from a lockfile).
    pub(crate) fn record_extension_result(
        &self,
        result: crate::lockfile_extensions::ExtensionResult,
    ) {
        let mut results = self.extension_results.lock().expect("extension results");
        results.retain(|known| known.id != result.id);
        results.push(result);
    }

    pub fn mode(&self) -> LockfileMode {
        self.mode
    }

    /// Remember how each registry was spelled, for Bazel's messages.
    pub fn with_spellings(
        mut self,
        spellings: impl IntoIterator<Item = (Registry, String)>,
    ) -> Self {
        for (registry, raw) in spellings {
            if let Registry::Url(base) = registry {
                self.spellings.insert(base, raw);
            }
        }
        self
    }

    pub fn lockfile(&self) -> &Lockfile {
        &self.lockfile
    }

    fn hashes_mode(&self, registry: &Registry) -> HashesMode {
        if registry.local_path().is_some() {
            return HashesMode::Ignore;
        }
        match self.mode {
            LockfileMode::Off => HashesMode::Ignore,
            LockfileMode::Update => HashesMode::UseAndUpdate,
            LockfileMode::Refresh => HashesMode::UseImmutableAndUpdate,
            LockfileMode::Error => HashesMode::Enforce,
        }
    }

    /// Read one registry file; at most one transport request per URL and
    /// invocation (so bazel_registry.json is fetched once per registry).
    pub fn read(&self, registry: &Registry, relative: &str) -> Result<Fetch, String> {
        if let Some(store) = &self.proof_store {
            if let Some(path) = registry.local_path() {
                let path = crate::observed_fs::canonicalize(path).map_err(|e| e.to_string())?;
                if path.starts_with(store) {
                    return Err("proof registry metadata requires an unproved derived root".into());
                }
                return direct(registry, relative);
            }
            // A current lock checksum is the only authority for remote CAS
            // metadata. Never use a URL memo, fetch, refresh or populate CAS.
            let url = registry.download_location(relative)?;
            if self.mode == LockfileMode::Off
                || self.mode == LockfileMode::Refresh
                || relative.ends_with("/metadata.json")
            {
                return Err("registry metadata is unavailable in read-only proof mode".into());
            }
            return match self.lockfile.registry_file_hashes.get(&url) {
                Some(Some(hash)) => {
                    if hash.len() != 64
                        || !hash
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                    {
                        return Err("current registry checksum is malformed".into());
                    }
                    let path = self
                        .cas
                        .as_ref()
                        .ok_or("proof has no registry CAS")?
                        .join(hash);
                    if !crate::observed_fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file())
                    {
                        return Err("current registry checksum has no regular CAS entry".into());
                    }
                    let bytes = crate::observed_fs::read(&path).map_err(|e| e.to_string())?;
                    if sha256_hex(&bytes) != *hash {
                        return Err("current registry CAS checksum mismatch".into());
                    }
                    Ok(Fetch::Found(Arc::new(bytes)))
                }
                Some(None) => Ok(Fetch::NotFound(format!(
                    "{url}: absent in current lockfile"
                ))),
                None => Err("current lockfile has no registry checksum for proof".into()),
            };
        }
        if registry.local_path().is_some() {
            return direct(registry, relative);
        }
        let url = registry.download_location(relative)?;
        let slot = self
            .memo
            .lock()
            .expect("registry memo")
            .entry(url.clone())
            .or_default()
            .clone();
        slot.get_or_init(|| self.fetch(registry, relative, &url))
            .clone()
    }

    fn fetch(&self, registry: &Registry, relative: &str, url: &str) -> Result<Fetch, String> {
        // metadata.json is not immutable (IndexRegistry.java:586-592).
        let hashed = !relative.ends_with("/metadata.json");
        if !hashed {
            return direct(registry, relative);
        }
        let mode = self.hashes_mode(registry);
        let known = if mode == HashesMode::Ignore {
            None
        } else {
            self.lockfile.registry_file_hashes.get(url)
        };
        let checksum = match known {
            None if mode == HashesMode::Enforce => {
                return Err(format!(
                    "Missing checksum for registry file {url} not permitted with --lockfile_mode=error. Please run `bazel mod deps --lockfile_mode=update` to update your lockfile."
                ));
            }
            None => None,
            Some(None) if mode == HashesMode::UseImmutableAndUpdate => None,
            Some(None) => {
                let result = Fetch::NotFound(format!(
                    "{url}: previously not found (as recorded in {}, refresh with --lockfile_mode=refresh)",
                    lockfile::LOCKFILE_NAME
                ));
                self.record(url, None);
                return Ok(result);
            }
            Some(Some(hash)) => Some(hash.as_str()),
        };
        if let Some(hash) = checksum
            && let Some(bytes) = self.cas_get(hash)
        {
            self.record(url, Some(hash.to_owned()));
            return Ok(Fetch::Found(Arc::new(bytes)));
        }
        // The transfer's network taints are settled when its bytes match the
        // lockfile's checksum: the next command reads those very bytes from
        // the content-addressed cache, without the network (and records the
        // same URL and hash).
        let transfer = crate::observed_fs::HeldTaint::network();
        let fetched = match crate::observed_fs::with_registry_sink(transfer.sink(), || {
            registry.read(relative)
        }) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                self.record(url, None);
                return Ok(Fetch::NotFound(format!("{url}: not found")));
            }
            Err(error) => {
                self.record(url, None);
                return Err(format!("Failed to fetch registry file {url}: {error}"));
            }
        };
        let actual = sha256_hex(&fetched);
        if let Some(wanted) = checksum
            && actual != wanted
        {
            // HashInputStream.check (:82-93), wrapped by IndexRegistry (:238-241).
            self.record(url, None);
            return Err(format!(
                "Failed to fetch registry file {url}: Checksum was {actual} but wanted {wanted}"
            ));
        }
        if self.mode != LockfileMode::Off {
            self.cas_put(&actual, &fetched);
        }
        if checksum.is_some() {
            transfer.settle();
        }
        self.record(url, Some(actual));
        Ok(Fetch::Found(Arc::new(fetched)))
    }

    /// A registry file's bytes for a module-root warm-up
    /// (crate::module_root_warm), without recording or memoizing anything:
    /// a file the sequential engine reads later is fetched, checked and
    /// recorded by that read exactly as without the warm-up (from the
    /// content-addressed cache this fills, when the lockfile knows its
    /// hash). `None` when the bytes cannot be obtained so (not found,
    /// a failure, a checksum the lockfile does not know in error mode):
    /// the warm-up gives up and the sequential read reports it.
    fn peek(&self, registry: &Registry, relative: &str) -> Option<Arc<Vec<u8>>> {
        if self.proof_store.is_some() {
            return None;
        }
        let url = registry.download_location(relative).ok()?;
        if let Some(slot) = self.memo.lock().expect("registry memo").get(&url)
            && let Some(result) = slot.get()
        {
            return match result {
                Ok(Fetch::Found(bytes)) => Some(bytes.clone()),
                _ => None,
            };
        }
        if relative.ends_with("/metadata.json") {
            return None;
        }
        let mode = self.hashes_mode(registry);
        let known = if mode == HashesMode::Ignore {
            None
        } else {
            self.lockfile.registry_file_hashes.get(&url)
        };
        let checksum = match known {
            Some(Some(hash)) => Some(hash.as_str()),
            None if mode != HashesMode::Enforce => None,
            _ => return None,
        };
        if let Some(hash) = checksum
            && let Some(bytes) = self.cas_get(hash)
        {
            return Some(Arc::new(bytes));
        }
        // Settled like [`Self::fetch`]'s: bytes the lockfile's checksum
        // names are what the sequential read takes from the cache.
        let transfer = crate::observed_fs::HeldTaint::network();
        let fetched =
            crate::observed_fs::with_registry_sink(transfer.sink(), || registry.read(relative))
                .ok()??;
        let actual = sha256_hex(&fetched);
        if checksum.is_some_and(|wanted| wanted != actual) {
            return None;
        }
        if self.mode != LockfileMode::Off {
            self.cas_put(&actual, &fetched);
        }
        if checksum.is_some() {
            transfer.settle();
        }
        Some(Arc::new(fetched))
    }

    fn record(&self, url: &str, hash: Option<String>) {
        if self.mode != LockfileMode::Off {
            self.recorded
                .lock()
                .expect("registry records")
                .insert(url.to_owned(), hash);
        }
    }

    fn cas_get(&self, hash: &str) -> Option<Vec<u8>> {
        let native = self.cas_get_native(hash);
        if native.is_some() {
            return native;
        }
        // The shared `--repository_cache` (genuine's layout,
        // crate::repository_cache): DownloadManager reads registry files
        // with a known checksum from the repository cache.
        let digest = crate::extension_download::Algorithm::Sha256;
        let bytes = crate::repository_cache::get_bytes(digest, &decode_hex(hash)?)?;
        self.cas_put_native(hash, &bytes);
        Some(bytes)
    }

    fn cas_get_native(&self, hash: &str) -> Option<Vec<u8>> {
        let path = self.cas.as_ref()?.join(hash);
        // snapshot-unrecorded: content-addressed blob, checked against its hash below; a miss is a network fetch, which taints (PLAN-L2 §1b).
        let metadata = std::fs::symlink_metadata(&path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        // snapshot-unrecorded: content-addressed blob (see above).
        let bytes = std::fs::read(&path).ok()?;
        (sha256_hex(&bytes) == hash).then_some(bytes)
    }

    /// Best effort, like DownloadCache.put after a download; also into the
    /// shared `--repository_cache` (crate::repository_cache).
    fn cas_put(&self, hash: &str, bytes: &[u8]) {
        if let Some(digest) = decode_hex(hash) {
            let _ = crate::repository_cache::put_bytes(
                crate::extension_download::Algorithm::Sha256,
                &digest,
                bytes,
            );
        }
        self.cas_put_native(hash, bytes);
    }

    fn cas_put_native(&self, hash: &str, bytes: &[u8]) {
        let Some(directory) = &self.cas else {
            return;
        };
        let path = directory.join(hash);
        if self.cas_get_native(hash).is_some() {
            return;
        }
        let publish = || -> std::io::Result<()> {
            std::fs::create_dir_all(directory)?;
            // snapshot-unrecorded: publishing a fetched blob (the fetch taints).
            if std::fs::symlink_metadata(directory)?
                .file_type()
                .is_symlink()
            {
                return Err(std::io::Error::other("symlink cache directory"));
            }
            let mut temp = tempfile::NamedTempFile::new_in(directory)?;
            temp.write_all(bytes)?;
            temp.persist(&path).map_err(|error| error.error)?;
            Ok(())
        };
        let _ = publish();
    }

    /// Fetch every registry file the lockfile names with a checksum
    /// (`registryFileHashes`: MODULE.bazel and source.json files) that the
    /// content-addressed cache lacks, all at once, into that cache only —
    /// nothing is memoized or recorded. Module discovery reads a horizon at
    /// a time (each level of the dependency graph waits for the previous
    /// one: ~/code's 386 MODULE.bazel files took 5 s on a cold cache); with
    /// the files the lockfile already names in the cache, its sequential
    /// reads find them by checksum, exactly as on a warm cache (recorded the
    /// same way, the same bytes). A file the lockfile names but discovery
    /// no longer reads costs one unused transfer; a transfer whose bytes do
    /// not match the checksum is dropped (the sequential read fetches and
    /// reports it).
    pub fn warm_known_files(&self, registries: &[Registry]) {
        if self.proof_store.is_some()
            || matches!(self.mode, LockfileMode::Off)
            || self.cas.is_none()
            // An A/B reference for diagnostics.
            || std::env::var_os("BAZEL_RS_DEBUG_NO_REGISTRY_WARM").is_some()
        {
            return;
        }
        let mut items: Vec<(Registry, String, String)> = Vec::new();
        {
            let memo = self.memo.lock().expect("registry memo");
            for (url, hash) in &self.lockfile.registry_file_hashes {
                let Some(hash) = hash else {
                    continue;
                };
                if !(url.ends_with("/MODULE.bazel") || url.ends_with("/source.json"))
                    || hash.len() != 64
                    || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                    || memo.contains_key(url)
                    || self.cas_has(hash)
                {
                    continue;
                }
                let found = registries.iter().find_map(|registry| {
                    let Registry::Url(base) = registry else {
                        return None;
                    };
                    let relative = url
                        .strip_prefix(base.trim_end_matches('/'))?
                        .strip_prefix('/')?;
                    (registry.download_location(relative).ok()? == *url)
                        .then(|| (registry.clone(), relative.to_owned()))
                });
                if let Some((registry, relative)) = found {
                    items.push((registry, relative, hash.clone()));
                }
            }
        }
        // A handful of files gains nothing over discovery's own horizons.
        if items.len() < WARM_MIN_FILES {
            return;
        }
        let workers = items.len().min(PARALLEL_FETCHES);
        let queue = Mutex::new(VecDeque::from(items));
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let next = queue.lock().expect("warm queue").pop_front();
                        let Some((registry, relative, hash)) = next else {
                            break;
                        };
                        // Settled either way: the bytes the lockfile's
                        // checksum names are what the sequential read takes
                        // from the cache, and a failed transfer leaves
                        // nothing behind (the sequential read fetches, and
                        // taints, itself).
                        let transfer = crate::observed_fs::HeldTaint::network();
                        let fetched =
                            crate::observed_fs::with_registry_sink(transfer.sink(), || {
                                registry.read(&relative)
                            });
                        if let Ok(Some(bytes)) = fetched
                            && sha256_hex(&bytes) == hash
                        {
                            self.cas_put(&hash, &bytes);
                        }
                        transfer.settle();
                    }
                });
            }
        });
    }

    /// Whether the native content-addressed cache holds an entry for `hash`
    /// (its bytes are verified when read), or the shared
    /// `--repository_cache` has it (copied into the native cache, as the
    /// sequential read would).
    fn cas_has(&self, hash: &str) -> bool {
        let Some(directory) = &self.cas else {
            return false;
        };
        // snapshot-unrecorded: a scheduling probe of the content-addressed cache; the read that uses an entry verifies it.
        std::fs::symlink_metadata(directory.join(hash)).is_ok_and(|metadata| metadata.is_file())
            || (crate::repository_cache::path().is_some() && self.cas_get(hash).is_some())
    }

    /// Fetch files concurrently into the memo; results (including errors)
    /// are observed later by sequential reads, so diagnostics stay
    /// deterministic.
    pub fn prefetch(&self, items: Vec<(Registry, String)>) {
        self.prefetch_fallbacks(
            items
                .into_iter()
                .map(|(registry, relative)| (vec![registry], relative))
                .collect(),
        );
    }

    /// Like [`Self::prefetch`], trying each item's registries in order until
    /// one does not report "not found", as ModuleFileFunction does for each
    /// module of a horizon in parallel (ModuleFileFunction.java:820-855).
    pub fn prefetch_fallbacks(&self, items: Vec<(Vec<Registry>, String)>) {
        if self.proof_store.is_some() {
            return;
        }
        let mut seen = BTreeSet::new();
        let pending: VecDeque<(Vec<Registry>, String)> = {
            let memo = self.memo.lock().expect("registry memo");
            items
                .into_iter()
                .filter(|(registries, relative)| {
                    registries.first().is_some_and(|registry| {
                        registry.local_path().is_none()
                            && registry
                                .download_location(relative)
                                .is_ok_and(|url| !memo.contains_key(&url) && seen.insert(url))
                    })
                })
                .collect()
        };
        if pending.is_empty() {
            return;
        }
        let workers = pending.len().min(PARALLEL_FETCHES);
        let queue = Mutex::new(pending);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    loop {
                        let next = queue.lock().expect("prefetch queue").pop_front();
                        let Some((registries, relative)) = next else {
                            break;
                        };
                        for registry in registries {
                            if registry.local_path().is_some()
                                || !matches!(
                                    self.read(&registry, &relative),
                                    Ok(Fetch::NotFound(_))
                                )
                            {
                                break;
                            }
                        }
                    }
                });
            }
        });
    }

    pub fn yanked_lookup(
        &self,
        registry: &Registry,
        module_key: &str,
        source_json_url: &str,
    ) -> YankedLookup {
        let mode = self.hashes_mode(registry);
        if mode == HashesMode::UseImmutableAndUpdate {
            return YankedLookup::Fetch;
        }
        if let Some((_, info)) = self
            .lockfile
            .selected_yanked_versions
            .iter()
            .find(|(key, _)| key == module_key)
        {
            return YankedLookup::Known(Some(info.clone()));
        }
        if mode == HashesMode::Enforce {
            return YankedLookup::Known(None);
        }
        if self
            .lockfile
            .registry_file_hashes
            .contains_key(source_json_url)
        {
            return YankedLookup::Known(None);
        }
        YankedLookup::Fetch
    }

    pub fn yanked_allowed(&self, module_key: &str) -> bool {
        self.allowed_yanked
            .as_ref()
            .is_none_or(|allowed| allowed.contains(module_key))
    }

    /// Resolution succeeded: BazelModuleResolutionValue exists.
    pub fn record_resolution(&self, resolution: Resolution) {
        *self.resolution.lock().expect("resolution record") = Some(resolution);
    }

    /// The loading phase — module resolution and every demanded module
    /// extension resolution (evaluation or lockfile reuse) — succeeded: the
    /// SingleExtensionValues BazelLockFileModule.afterCommand collects,
    /// "evaluated in the current build" (:105-141), exist for this
    /// invocation and the lockfile may be written. A command that fails
    /// before this point — extension evaluation included — leaves
    /// MODULE.bazel.lock untouched (stricter than Bazel, see
    /// `extensions_evaluated`). `resolved` only feeds the write-path
    /// diagnostic.
    pub fn mark_module_extensions_resolved(&self, resolved: usize) {
        self.resolved_extensions.store(resolved, Ordering::Release);
        self.extensions_evaluated.store(true, Ordering::Release);
    }

    /// The lockfile BazelLockFileModule.afterCommand would write, if any.
    pub fn updated_lockfile(&self) -> Option<Lockfile> {
        if !self.mode.writes() {
            return None;
        }
        if !self.extensions_evaluated.load(Ordering::Acquire) {
            return None;
        }
        let resolution = self.resolution.lock().expect("resolution record").clone()?;
        let usages = resolution.usages?;
        let registry_file_hashes = self
            .recorded
            .lock()
            .expect("registry records")
            .iter()
            .filter(|(url, _)| !url.starts_with("file:"))
            .map(|(url, hash)| (url.clone(), hash.clone()))
            .collect();
        let results = self
            .extension_results
            .lock()
            .expect("extension results")
            .clone();
        let has_usages = |id: &str| usages.contains(id);
        let (facts, facts_versions) = crate::lockfile_extensions::combine_facts(
            &self.lockfile.facts,
            &self.lockfile.facts_versions,
            &results,
            &has_usages,
        );
        let new = Lockfile {
            version: lockfile::LOCK_FILE_VERSION,
            registry_file_hashes,
            selected_yanked_versions: resolution.selected_yanked,
            module_extensions: crate::lockfile_extensions::combine_extensions(
                &self.lockfile.module_extensions,
                &results,
                &has_usages,
                false,
            ),
            facts,
            facts_versions,
        };
        (new != self.lockfile).then_some(new)
    }

    /// The hidden lockfile BazelLockFileModule.afterCommand would write
    /// (:206-226): reproducible extension results and the facts of this
    /// invocation's results, never the workspace lockfile's. Gated exactly
    /// like `updated_lockfile` (fail-closed; stricter than Bazel) and never
    /// written by `--native-lockfile=readonly`, which reads only.
    pub fn updated_hidden_lockfile(&self) -> Option<(PathBuf, Lockfile)> {
        if !self.mode.writes() || self.readonly {
            return None;
        }
        if !self.extensions_evaluated.load(Ordering::Acquire) {
            return None;
        }
        let (path, old) = self.hidden.as_ref()?;
        let resolution = self.resolution.lock().expect("resolution record").clone()?;
        let usages = resolution.usages?;
        let results = self
            .extension_results
            .lock()
            .expect("extension results")
            .clone();
        let has_usages = |id: &str| usages.contains(id);
        let (facts, facts_versions) = crate::lockfile_extensions::combine_facts(
            &old.facts,
            &old.facts_versions,
            &results,
            &has_usages,
        );
        let new = Lockfile {
            version: lockfile::LOCK_FILE_VERSION,
            registry_file_hashes: BTreeMap::new(),
            selected_yanked_versions: Vec::new(),
            module_extensions: crate::lockfile_extensions::combine_extensions(
                &old.module_extensions,
                &results,
                &has_usages,
                true,
            ),
            facts,
            facts_versions,
        };
        (self.hidden_seeded || new != *old).then(|| (path.clone(), new))
    }

    /// Write the updated lockfiles, if any: the workspace lockfile to the
    /// configured target and the hidden lockfile. Like
    /// BazelLockFileModule.updateLockfile, failures do not fail the command.
    /// The native-only diagnostic of how many module extensions the
    /// invocation resolved (evaluated or reused from an up-to-date lockfile
    /// entry) prints only after workspace-lockfile bytes were actually
    /// written (write-only-if-changed; Bazel prints nothing).
    pub fn finish(&self) -> Option<String> {
        let mut warnings = Vec::new();
        if let Some((path, hidden)) = self.updated_hidden_lockfile() {
            // Written to a temporary file and renamed into place: another
            // worktree may read it as its seed at any time.
            let temporary = path.with_extension(format!("lock.tmp-{}", std::process::id()));
            let written = path
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(&temporary, hidden.serialize()))
                .and_then(|()| std::fs::rename(&temporary, &path))
                .inspect_err(|_| {
                    let _ = std::fs::remove_file(&temporary);
                });
            if let Err(error) = written {
                warnings.push(format!(
                    "Error while updating the hidden MODULE.bazel.lock file: {error}"
                ));
            }
        }
        if let (Some(path), Some(lockfile)) = (self.write_path.as_ref(), self.updated_lockfile()) {
            match std::fs::write(path, lockfile.serialize()) {
                Ok(()) => {
                    let resolved = self.resolved_extensions.load(Ordering::Acquire);
                    if resolved > 0 {
                        crate::note!(
                            "bazel-rs: resolved {resolved} module extension(s) for MODULE.bazel.lock"
                        );
                    }
                }
                Err(error) => warnings.push(format!(
                    "Error while updating MODULE.bazel.lock file: {error}"
                )),
            }
        }
        (!warnings.is_empty()).then(|| warnings.join("\n"))
    }
}

/// How a hidden lockfile was read without being observed
/// ([`RegistrySession::note_hidden_lockfile`]).
#[derive(Debug)]
enum HiddenRead {
    /// From its own path, whose stat key was `key` before the read.
    Own { key: crate::observed_fs::StatKey },
    /// From a seed (its own path absent): the seed's path, stat key and
    /// bytes' SHA-256.
    Seed {
        path: PathBuf,
        key: crate::observed_fs::StatKey,
        sha256: [u8; 32],
    },
    /// From a seed that existed but could not be read (the empty lockfile).
    SeedUnreadable(PathBuf),
}

/// The native hidden lockfile of `workspace` under `--cache-dir` (Bazel:
/// `<output_base>/MODULE.bazel.lock`; an output base is per workspace): in
/// the workspace's state directory, `<cache>/worktrees/<name>-<hash>/`
/// (crate::worktree).
pub fn hidden_lockfile_path(cache_dir: &Path, workspace: &Path) -> PathBuf {
    crate::worktree::output_base_for(cache_dir, workspace).join(lockfile::LOCKFILE_NAME)
}

/// Where the hidden lockfile of `workspace` was before per-worktree state
/// (`<cache>/hidden-lockfiles/<sha256 of the workspace>/`): a seed of the
/// new one ([`RegistrySession::with_seeded_hidden_lockfile`]).
pub fn legacy_hidden_lockfile_path(cache_dir: &Path, workspace: &Path) -> PathBuf {
    let canonical =
        crate::observed_fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    cache_dir
        .join("hidden-lockfiles")
        .join(sha256_hex(canonical.as_os_str().as_encoded_bytes()))
        .join(lockfile::LOCKFILE_NAME)
}

/// `lockfile` without the module extension results and facts whose JSON
/// names the `foreign` workspace's path (a seed from another workspace,
/// crate::worktree::Seed: such a result — a repository attribute holding a
/// path below that workspace — would point this workspace's repositories
/// into the other one). A path into the cache both share names nothing
/// ([`crate::worktree::WorkspaceMentions`]).
pub fn without_mentions(
    mut lockfile: Lockfile,
    foreign: &crate::worktree::WorkspaceMentions,
) -> Lockfile {
    let named = |text: &str| foreign.named_in(text.as_bytes());
    lockfile
        .module_extensions
        .retain(|(id, entry)| !named(id) && !mentions(entry, &named));
    lockfile
        .facts
        .retain(|(id, facts)| !named(id) && !mentions(facts, &named));
    // A facts version without its facts (combine_facts keeps none either).
    let facts = &lockfile.facts;
    lockfile
        .facts_versions
        .retain(|(id, _)| facts.iter().any(|(kept, _)| kept == id));
    lockfile
}

fn mentions(json: &lockfile::Json, named: &impl Fn(&str) -> bool) -> bool {
    use lockfile::Json;
    match json {
        Json::String(text) => named(text),
        Json::Array(items) => items.iter().any(|item| mentions(item, named)),
        Json::Object(members) => members
            .iter()
            .any(|(key, value)| named(key) || mentions(value, named)),
        Json::Null | Json::Bool(_) | Json::Number(_) => false,
    }
}

impl SessionHandle {
    /// A private fresh lockfile view for non-materializing preparation. Reopen
    /// the current overlay/workspace choice; never inherit evaluated extension
    /// results, memoized registry reads or seeded historical hidden entries.
    pub(crate) fn for_publication_proof(
        &self,
        workspace: &Path,
        store: &Path,
    ) -> Result<Self, String> {
        let proof_store =
            crate::observed_fs::canonicalize(store).unwrap_or_else(|_| store.to_path_buf());
        let Some(session) = &self.0 else {
            let mut session = RegistrySession::open(
                workspace,
                LockfileMode::Off,
                &LockfileTarget::ReadOnly,
                None,
                Default::default(),
            )?;
            session.proof_store = Some(proof_store);
            return Ok(Self::new(session));
        };
        if session.mode == LockfileMode::Refresh {
            return Err("lockfile refresh cannot be proved without evaluation".into());
        }
        if session.hidden_seeded {
            return Err("publication proof does not support seeded hidden lockfiles".into());
        }
        let mut fresh = RegistrySession::open(
            &session.workspace,
            session.mode,
            &session.target,
            None,
            session.allowed_yanked.clone(),
        )?;
        fresh.readonly = true;
        fresh.proof_store = Some(proof_store);
        fresh.cas = session.cas.clone();
        fresh.spellings = session.spellings.clone();
        if let Some((path, _)) = &session.hidden {
            fresh = fresh.with_observed_hidden_lockfile(path.clone());
        }
        Ok(Self::new(fresh))
    }

    pub(crate) fn publication_proof(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|session| session.proof_store.is_some())
    }

    pub fn new(session: RegistrySession) -> Self {
        Self(Some(Arc::new(session)))
    }

    pub fn session(&self) -> Option<&RegistrySession> {
        self.0.as_deref()
    }

    pub fn mode(&self) -> LockfileMode {
        self.0.as_ref().map_or(LockfileMode::Off, |s| s.mode)
    }

    /// The registry as Bazel's IndexRegistry.getUrl() prints it: the
    /// spelling given on the command line (or the default), else the URL
    /// without a trailing slash.
    pub fn registry_display(&self, registry: &Registry) -> String {
        if let (Some(session), Registry::Url(base)) = (&self.0, registry)
            && let Some(raw) = session.spellings.get(base)
        {
            return raw.clone();
        }
        registry.bazel_url()
    }

    /// Without a session, reads are direct and uncached (library default).
    pub fn read(&self, registry: &Registry, relative: &str) -> Result<Fetch, String> {
        match &self.0 {
            Some(session) => session.read(registry, relative),
            None => direct(registry, relative),
        }
    }

    pub fn prefetch(&self, items: Vec<(Registry, String)>) {
        if let Some(session) = &self.0 {
            session.prefetch(items);
        }
    }

    pub fn prefetch_fallbacks(&self, items: Vec<(Vec<Registry>, String)>) {
        if let Some(session) = &self.0 {
            session.prefetch_fallbacks(items);
        }
    }

    /// [`RegistrySession::peek`] parsed like [`Self::read_object`]: `None`
    /// for anything but a JSON object.
    pub(crate) fn peek_object(
        &self,
        registry: &Registry,
        relative: &str,
    ) -> Option<serde_json::Map<String, serde_json::Value>> {
        let bytes = match &self.0 {
            Some(session) => session.peek(registry, relative)?,
            None => match direct(registry, relative).ok()? {
                Fetch::Found(bytes) => bytes,
                Fetch::NotFound(_) => return None,
            },
        };
        match serde_json::from_slice(&bytes).ok()? {
            serde_json::Value::Object(object) => Some(object),
            _ => None,
        }
    }

    /// Blank documents are absent (IndexRegistry.grabJson, :321-333).
    pub fn read_object(
        &self,
        registry: &Registry,
        relative: &str,
    ) -> Result<Option<serde_json::Map<String, serde_json::Value>>, String> {
        let Fetch::Found(bytes) = self.read(registry, relative)? else {
            return Ok(None);
        };
        let text = std::str::from_utf8(&bytes).map_err(|_| {
            format!(
                "registry entry is not UTF-8: {}",
                registry.location(relative)
            )
        })?;
        if text.trim().is_empty() {
            return Ok(None);
        }
        match serde_json::from_str(text) {
            Ok(serde_json::Value::Object(object)) => Ok(Some(object)),
            Ok(_) => Err(format!(
                "registry metadata must be a JSON object: {}",
                registry.location(relative)
            )),
            Err(error) => Err(format!(
                "invalid registry JSON {}: {error}",
                registry.location(relative)
            )),
        }
    }
}

fn direct(registry: &Registry, relative: &str) -> Result<Fetch, String> {
    Ok(match registry.read(relative)? {
        Some(bytes) => Fetch::Found(Arc::new(bytes)),
        None => Fetch::NotFound(format!("{}: not found", registry.location(relative))),
    })
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// YankedVersionsUtil.parseAllowedYankedVersions (:30-108): the
/// BZLMOD_ALLOW_YANKED_VERSIONS environment value first, then each
/// `--allow_yanked_versions`; `all` allows everything.
pub fn parse_allowed_yanked(env: Option<&str>, flags: &[String]) -> Result<AllowedYanked, String> {
    let mut allowed = BTreeSet::new();
    let mut parse = |input: &str, context: String| -> Result<bool, String> {
        for item in input.split(',') {
            if item == "all" {
                return Ok(true);
            }
            if item.is_empty() {
                continue;
            }
            let Some((name, version)) = item.split_once('@') else {
                return Err(format!(
                    "Parsing {context} failed, module versions must be of the form '<module name>@<version>'"
                ));
            };
            if !valid_module_name(name) {
                return Err(format!(
                    "Parsing {context} failed, invalid module name '{name}': valid names must 1) only contain lowercase letters (a-z), digits (0-9), dots (.), hyphens (-), and underscores (_); 2) begin with a lowercase letter; 3) end with a lowercase letter or digit."
                ));
            }
            let normalized = crate::module_graph::normalize_declared_version(version).map_err(|error| {
                format!("Parsing {context} failed, invalid version specified for module: {version}: {error}")
            })?;
            allowed.insert(format!(
                "{name}@{}",
                if normalized.is_empty() {
                    "_"
                } else {
                    &normalized
                }
            ));
        }
        Ok(false)
    };
    if let Some(env) = env
        && parse(
            env,
            format!("environment variable BZLMOD_ALLOW_YANKED_VERSIONS={env}"),
        )?
    {
        return Ok(None);
    }
    for flag in flags {
        if parse(
            flag,
            format!("command line flag --allow_yanked_versions={flag}"),
        )? {
            return Ok(None);
        }
    }
    Ok(Some(allowed))
}

/// RepositoryName.VALID_MODULE_NAME `[a-z]([a-z0-9._-]*[a-z0-9])?`.
fn valid_module_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
}

/// Lowercase hex to bytes (a registry file's recorded SHA-256).
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(text.get(index..index + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn proof_registry_reads_only_current_lock_checksums_with_zero_transport() {
        let temp = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let server = serve(BTreeMap::new());
        let registry = Registry::parse(&server.base).unwrap();
        let relative = "modules/defs/1.0/MODULE.bazel";
        let url = registry.download_location(relative).unwrap();
        let a = b"module(name='defs',version='1.0')";
        let b = b"module(name='defs',version='1.1')";
        let cache = temp.path().join("cache");
        let cas = crate::materializer::download_cache_dir(&cache, "sha256");
        std::fs::create_dir_all(&cas).unwrap();
        std::fs::write(cas.join(sha256_hex(a)), a).unwrap();
        let lock = |hash: &str| {
            serde_json::json!({"lockFileVersion":28,"registryFileHashes":{url.clone():hash}})
                .to_string()
        };
        let old = SessionHandle::new(session(
            temp.path(),
            LockfileMode::Update,
            Some(&lock(&sha256_hex(a))),
        ));
        let proof = old.for_publication_proof(temp.path(), &cache).unwrap();
        assert!(
            matches!(proof.read(&registry, relative).unwrap(), Fetch::Found(bytes) if bytes.as_slice() == a)
        );
        std::fs::write(temp.path().join("MODULE.bazel.lock"), lock(&sha256_hex(b))).unwrap();
        let proof = old.for_publication_proof(temp.path(), &cache).unwrap();
        assert!(
            proof.read(&registry, relative).is_err(),
            "old memo/CAS must not substitute for current checksum"
        );
        std::fs::write(cas.join(sha256_hex(b)), b).unwrap();
        assert!(
            matches!(proof.read(&registry, relative).unwrap(), Fetch::Found(bytes) if bytes.as_slice() == b)
        );
        std::fs::write(cas.join(sha256_hex(b)), b"tampered").unwrap();
        assert!(proof.read(&registry, relative).is_err());
        proof.prefetch(vec![(registry.clone(), relative.to_owned())]);
        assert!(proof.read(&registry, "missing").is_err());
        assert_eq!(server.total.load(Ordering::SeqCst), 0);
        assert_eq!(std::fs::read(cas.join(sha256_hex(b))).unwrap(), b"tampered");
    }

    /// A tiny HTTP/1.1 server over a map of paths; counts requests per path.
    struct Server {
        base: String,
        counts: Arc<Mutex<BTreeMap<String, usize>>>,
        total: Arc<AtomicUsize>,
    }

    fn serve(files: BTreeMap<String, Vec<u8>>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let counts = Arc::new(Mutex::new(BTreeMap::new()));
        let total = Arc::new(AtomicUsize::new(0));
        let files = Arc::new(files);
        let (c, t) = (counts.clone(), total.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (files, c, t) = (files.clone(), c.clone(), t.clone());
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        let path = line.split(' ').nth(1).unwrap_or("").to_owned();
                        loop {
                            let mut header = String::new();
                            reader.read_line(&mut header).unwrap();
                            if header == "\r\n" || header.is_empty() {
                                break;
                            }
                        }
                        t.fetch_add(1, Ordering::SeqCst);
                        *c.lock().unwrap().entry(path.clone()).or_default() += 1;
                        let response = match files.get(&path) {
                            Some(body) => {
                                let mut r = format!(
                                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                                    body.len()
                                )
                                .into_bytes();
                                r.extend_from_slice(body);
                                r
                            }
                            None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
                        };
                        let _ = stream.write_all(&response);
                    }
                });
            }
        });
        Server {
            base,
            counts,
            total,
        }
    }

    fn session(dir: &Path, mode: LockfileMode, lock: Option<&str>) -> RegistrySession {
        if let Some(lock) = lock {
            std::fs::write(dir.join("MODULE.bazel.lock"), lock).unwrap();
        }
        RegistrySession::open(
            dir,
            mode,
            &LockfileTarget::Workspace,
            Some(dir.join("cache")),
            Some(BTreeSet::new()),
        )
        .unwrap()
    }

    #[test]
    fn hashed_files_are_memoized_cached_by_hash_and_recorded() {
        let files = BTreeMap::from([
            (
                "/modules/a/1/MODULE.bazel".to_owned(),
                b"module(name='a', version='1')".to_vec(),
            ),
            ("/bazel_registry.json".to_owned(), b"{}".to_vec()),
        ]);
        let server = serve(files);
        let registry = Registry::parse(&server.base).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let first = session(dir.path(), LockfileMode::Update, None);
        for _ in 0..3 {
            assert!(matches!(
                first.read(&registry, "bazel_registry.json").unwrap(),
                Fetch::Found(_)
            ));
        }
        assert!(matches!(
            first.read(&registry, "modules/a/1/MODULE.bazel").unwrap(),
            Fetch::Found(_)
        ));
        assert!(
            matches!(first.read(&registry, "modules/b/1/MODULE.bazel").unwrap(), Fetch::NotFound(m) if m.ends_with("/modules/b/1/MODULE.bazel: not found"))
        );
        assert_eq!(server.total.load(Ordering::SeqCst), 3);
        first.record_resolution(Resolution {
            selected_yanked: Vec::new(),
            usages: Some(ExtensionUsages::default()),
        });
        first.mark_module_extensions_resolved(0);
        assert_eq!(first.finish(), None);
        let text = std::fs::read_to_string(dir.path().join("MODULE.bazel.lock")).unwrap();
        assert!(
            text.contains("/modules/b/1/MODULE.bazel\": \"not found\""),
            "{text}"
        );
        // Second invocation: known hashes come from the cache, recorded
        // "not found" is trusted, and an unchanged value is not rewritten.
        let second = session(dir.path(), LockfileMode::Update, None);
        assert!(matches!(
            second.read(&registry, "modules/a/1/MODULE.bazel").unwrap(),
            Fetch::Found(_)
        ));
        assert!(matches!(
            second.read(&registry, "bazel_registry.json").unwrap(),
            Fetch::Found(_)
        ));
        assert!(
            matches!(second.read(&registry, "modules/b/1/MODULE.bazel").unwrap(), Fetch::NotFound(m) if m.contains("previously not found (as recorded in MODULE.bazel.lock, refresh with --lockfile_mode=refresh)"))
        );
        assert_eq!(server.total.load(Ordering::SeqCst), 3);
        second.record_resolution(Resolution {
            selected_yanked: Vec::new(),
            usages: Some(ExtensionUsages::default()),
        });
        second.mark_module_extensions_resolved(0);
        assert!(second.updated_lockfile().is_none());
        // Refresh retries "not found"; error mode requires hashes.
        let refresh = session(dir.path(), LockfileMode::Refresh, None);
        assert!(
            matches!(refresh.read(&registry, "modules/b/1/MODULE.bazel").unwrap(), Fetch::NotFound(m) if m.ends_with(": not found"))
        );
        assert_eq!(
            server.counts.lock().unwrap()["/modules/b/1/MODULE.bazel"],
            2
        );
        let error = session(dir.path(), LockfileMode::Error, None);
        assert!(
            error
                .read(&registry, "modules/c/1/MODULE.bazel")
                .unwrap_err()
                .starts_with("Missing checksum for registry file http://")
        );
        assert!(matches!(
            error.read(&registry, "modules/a/1/MODULE.bazel").unwrap(),
            Fetch::Found(_)
        ));
    }

    #[test]
    fn tampered_registry_files_fail_with_bazel_checksum_text() {
        let server = serve(BTreeMap::from([(
            "/modules/a/1/MODULE.bazel".to_owned(),
            b"changed".to_vec(),
        )]));
        let registry = Registry::parse(&server.base).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let url = format!("{}modules/a/1/MODULE.bazel", server.base);
        let wanted = "0".repeat(64);
        let lock = format!(
            "{{\"lockFileVersion\": 28, \"registryFileHashes\": {{\"{url}\": \"{wanted}\"}}}}"
        );
        let session = session(dir.path(), LockfileMode::Update, Some(&lock));
        assert_eq!(
            session
                .read(&registry, "modules/a/1/MODULE.bazel")
                .unwrap_err(),
            format!(
                "Failed to fetch registry file {url}: Checksum was {} but wanted {wanted}",
                sha256_hex(b"changed")
            )
        );
    }

    #[test]
    fn parallel_prefetch_fetches_each_url_once() {
        let files: BTreeMap<_, _> = (0..40)
            .map(|i| (format!("/modules/m{i}/1/MODULE.bazel"), vec![b'x'; i]))
            .collect();
        let server = serve(files);
        let registry = Registry::parse(&server.base).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let session = session(dir.path(), LockfileMode::Off, None);
        let items: Vec<_> = (0..40)
            .chain(0..40)
            .map(|i| (registry.clone(), format!("modules/m{i}/1/MODULE.bazel")))
            .collect();
        session.prefetch(items);
        assert_eq!(server.total.load(Ordering::SeqCst), 40);
        for i in 0..40 {
            assert!(
                matches!(session.read(&registry, &format!("modules/m{i}/1/MODULE.bazel")).unwrap(), Fetch::Found(b) if b.len() == i)
            );
        }
        assert_eq!(server.total.load(Ordering::SeqCst), 40);
        // Off never writes.
        session.record_resolution(Resolution {
            selected_yanked: Vec::new(),
            usages: Some(ExtensionUsages::default()),
        });
        assert!(session.updated_lockfile().is_none());
    }

    /// The registry files MODULE.bazel.lock names with a checksum are
    /// fetched at once into the content-addressed cache only: nothing is
    /// recorded or memoized until a sequential read, which then needs no
    /// transfer; a file whose bytes do not match is not kept.
    #[test]
    fn known_registry_files_are_warmed_into_the_cache_only() {
        let files: BTreeMap<_, _> = (0..20)
            .map(|i| {
                (
                    format!("/modules/m{i}/1/MODULE.bazel"),
                    format!("module(name = 'm{i}')").into_bytes(),
                )
            })
            .chain([("/modules/m0/1/source.json".to_owned(), b"{}".to_vec())])
            .collect();
        let server = serve(files.clone());
        let registry = Registry::parse(&server.base).unwrap();
        let url = |path: &str| {
            registry
                .download_location(path.trim_start_matches('/'))
                .unwrap()
        };
        let mut hashes: BTreeMap<String, serde_json::Value> = files
            .iter()
            .map(|(path, bytes)| (url(path), sha256_hex(bytes).into()))
            .collect();
        // A wrong checksum, a recorded absence, an unhashed file type.
        hashes.insert(url("/modules/m1/1/MODULE.bazel"), "0".repeat(64).into());
        hashes.insert(url("/modules/gone/1/MODULE.bazel"), "not found".into());
        let lock = serde_json::json!({"lockFileVersion": 28, "registryFileHashes": hashes});
        let dir = tempfile::tempdir().unwrap();
        let session = session(dir.path(), LockfileMode::Update, Some(&lock.to_string()));
        session.warm_known_files(std::slice::from_ref(&registry));
        // Every hashed file once (m1 too: its mismatch is found by fetching).
        assert_eq!(server.total.load(Ordering::SeqCst), 21);
        assert!(session.recorded.lock().unwrap().is_empty());
        assert!(session.memo.lock().unwrap().is_empty());
        for i in [0, 2, 19] {
            let relative = format!("modules/m{i}/1/MODULE.bazel");
            assert!(matches!(
                session.read(&registry, &relative).unwrap(),
                Fetch::Found(bytes) if *bytes == files[&format!("/{relative}")]
            ));
        }
        assert_eq!(
            server.total.load(Ordering::SeqCst),
            21,
            "reads hit the cache"
        );
        // The mismatching file is fetched (and reported) by its read.
        assert!(
            session
                .read(&registry, "modules/m1/1/MODULE.bazel")
                .is_err()
        );
        assert_eq!(server.total.load(Ordering::SeqCst), 22);
        // A second warm-up finds everything cached or already read.
        session.warm_known_files(std::slice::from_ref(&registry));
        assert_eq!(server.total.load(Ordering::SeqCst), 22);
    }

    #[test]
    fn yanked_lookup_follows_index_registry() {
        let dir = tempfile::tempdir().unwrap();
        let registry = Registry::parse("https://r.example").unwrap();
        let source = "https://r.example/modules/a/1/source.json";
        let lock = format!(
            "{{\"lockFileVersion\": 28, \"registryFileHashes\": {{\"{source}\": \"not found\"}}, \"selectedYankedVersions\": {{\"y@1\": \"bad\"}}}}"
        );
        let update = session(dir.path(), LockfileMode::Update, Some(&lock));
        assert_eq!(
            update.yanked_lookup(&registry, "a@1", source),
            YankedLookup::Known(None)
        );
        assert_eq!(
            update.yanked_lookup(&registry, "y@1", "x"),
            YankedLookup::Known(Some("bad".into()))
        );
        assert_eq!(
            update.yanked_lookup(&registry, "b@1", "x"),
            YankedLookup::Fetch
        );
        let refresh = session(dir.path(), LockfileMode::Refresh, None);
        assert_eq!(
            refresh.yanked_lookup(&registry, "y@1", "x"),
            YankedLookup::Fetch
        );
        let error = session(dir.path(), LockfileMode::Error, None);
        assert_eq!(
            error.yanked_lookup(&registry, "b@1", "x"),
            YankedLookup::Known(None)
        );
    }

    #[test]
    fn allowed_yanked_versions_parse_like_bazel() {
        assert_eq!(
            parse_allowed_yanked(None, &[]).unwrap(),
            Some(BTreeSet::new())
        );
        assert_eq!(
            parse_allowed_yanked(Some("a@1.0,b@2"), &["c@3".into()])
                .unwrap()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            parse_allowed_yanked(None, &["a@1,all".into()]).unwrap(),
            None
        );
        assert_eq!(
            parse_allowed_yanked(None, &["a".into()]).unwrap_err(),
            "Parsing command line flag --allow_yanked_versions=a failed, module versions must be of the form '<module name>@<version>'"
        );
        assert!(parse_allowed_yanked(Some("A@1"), &[]).unwrap_err().starts_with("Parsing environment variable BZLMOD_ALLOW_YANKED_VERSIONS=A@1 failed, invalid module name 'A'"));
    }

    #[test]
    fn lockfile_targets() {
        assert_eq!(
            LockfileTarget::parse("readonly").unwrap(),
            LockfileTarget::ReadOnly
        );
        assert!(LockfileTarget::parse("relative/path").is_err());
        let dir = tempfile::tempdir().unwrap();
        let overlay = dir.path().join("private.lock");
        std::fs::write(
            dir.path().join("MODULE.bazel.lock"),
            "{\"lockFileVersion\": 28, \"registryFileHashes\": {\"https://x/a\": \"not found\"}}",
        )
        .unwrap();
        let session = RegistrySession::open(
            dir.path(),
            LockfileMode::Update,
            &LockfileTarget::Overlay(overlay.clone()),
            None,
            None,
        )
        .unwrap();
        assert_eq!(session.lockfile().registry_file_hashes.len(), 1);
        session.record_resolution(Resolution {
            selected_yanked: Vec::new(),
            usages: Some(ExtensionUsages::default()),
        });
        // BazelLockFileModule.afterCommand writes nothing before every
        // usage-table extension evaluated (BazelLockFileModule.java:95-100).
        assert_eq!(session.finish(), None);
        assert!(!overlay.exists());
        session.mark_module_extensions_resolved(0);
        assert_eq!(session.finish(), None);
        assert!(overlay.exists());
        assert!(
            std::fs::read_to_string(dir.path().join("MODULE.bazel.lock"))
                .unwrap()
                .contains("https://x/a")
        );
        let readonly = RegistrySession::open(
            dir.path(),
            LockfileMode::Update,
            &LockfileTarget::ReadOnly,
            None,
            None,
        )
        .unwrap();
        readonly.record_resolution(Resolution {
            selected_yanked: Vec::new(),
            usages: Some(ExtensionUsages::default()),
        });
        assert!(readonly.updated_lockfile().is_none());
        readonly.mark_module_extensions_resolved(0);
        assert!(readonly.updated_lockfile().is_some());
        assert_eq!(readonly.finish(), None);
    }
}
