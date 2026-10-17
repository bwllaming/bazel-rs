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
use std::sync::{Arc, Mutex, OnceLock};

/// Concurrent registry requests (Bazel's discovery fetches a whole horizon
/// in parallel through Skyframe, Discovery.java:113-137).
const PARALLEL_FETCHES: usize = 32;

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
    lockfile: Lockfile,
    write_path: Option<PathBuf>,
    cas: Option<PathBuf>,
    allowed_yanked: AllowedYanked,
    memo: Mutex<HashMap<String, Arc<FetchSlot>>>,
    recorded: Mutex<BTreeMap<String, Option<String>>>,
    resolution: Mutex<Option<Resolution>>,
    /// Normalized registry URL -> spelling as given (IndexRegistry.getUrl()).
    spellings: BTreeMap<String, String>,
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
                if path.exists() {
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
            lockfile,
            write_path,
            cas: repository_cache.map(|cache| cache.join("downloads").join("sha256")),
            allowed_yanked,
            memo: Mutex::default(),
            recorded: Mutex::default(),
            resolution: Mutex::default(),
            spellings: BTreeMap::new(),
        })
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
        let fetched = match registry.read(relative) {
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
        self.record(url, Some(actual));
        Ok(Fetch::Found(Arc::new(fetched)))
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
        let path = self.cas.as_ref()?.join(hash);
        let metadata = std::fs::symlink_metadata(&path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        let bytes = std::fs::read(&path).ok()?;
        (sha256_hex(&bytes) == hash).then_some(bytes)
    }

    /// Best effort, like DownloadCache.put after a download.
    fn cas_put(&self, hash: &str, bytes: &[u8]) {
        let Some(directory) = &self.cas else {
            return;
        };
        let path = directory.join(hash);
        if self.cas_get(hash).is_some() {
            return;
        }
        let publish = || -> std::io::Result<()> {
            std::fs::create_dir_all(directory)?;
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

    /// The lockfile BazelLockFileModule.afterCommand would write, if any.
    pub fn updated_lockfile(&self) -> Option<Lockfile> {
        if !self.mode.writes() {
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
        let mut new = Lockfile {
            version: lockfile::LOCK_FILE_VERSION,
            registry_file_hashes,
            selected_yanked_versions: resolution.selected_yanked,
            module_extensions: self.lockfile.module_extensions.clone(),
            facts: self.lockfile.facts.clone(),
            facts_versions: self.lockfile.facts_versions.clone(),
        };
        new.prune_extensions(|id| usages.contains(id));
        (new != self.lockfile).then_some(new)
    }

    /// Write the updated lockfile, if any, to the configured target. Like
    /// BazelLockFileModule.updateLockfile, failures do not fail the command.
    pub fn finish(&self) -> Option<String> {
        let path = self.write_path.as_ref()?;
        let lockfile = self.updated_lockfile()?;
        std::fs::write(path, lockfile.serialize())
            .err()
            .map(|error| format!("Error while updating MODULE.bazel.lock file: {error}"))
    }
}

impl SessionHandle {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        assert!(readonly.updated_lockfile().is_some());
        assert_eq!(readonly.finish(), None);
    }
}
