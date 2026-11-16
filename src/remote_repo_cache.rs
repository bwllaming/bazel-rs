//! The remote repository contents cache
//! (`--experimental_remote_repo_contents_cache`, docs/REPO_CONTENTS_CACHE.md).
//!
//! Bazel 9.3.1 (`remote/RemoteRepoContentsCacheImpl.java`) stores the
//! fetched contents of reproducible repositories in the ordinary remote
//! cache: an Action Cache entry of a synthetic, never executed action whose
//! salt is the repository's key, holding the repository directory as an
//! output directory (a `Tree`) and its recorded inputs as an output file.
//! A repository fetch that misses locally looks the entry up, re-checks the
//! recorded inputs against the current world and takes the tree instead of
//! running the rule; a real fetch of an eligible repository uploads it.
//!
//! This module is the transport: the keys, the entries' encoding, the
//! lookups and transfers. The repository store decides when to look up and
//! what to publish (crate::repository_store::materialize_inner): a hit is
//! published exactly like an execution of the rule that produced the same
//! tree and runtime inputs.
//!
//! Entries (bazel-rs's own salt, never shared with genuine's, whose keys
//! are its predeclared-input hashes):
//! * intermediate, salt `spec:<specification key>`: `exit_code` 0 and a
//!   stdout blob listing, newest first, the SHA-256 of the recorded inputs
//!   of each final entry stored for the specification (genuine's
//!   intermediate entries list recorded-input batches; bazel-rs's recorded
//!   inputs are one opaque record, so a line names one record);
//! * final, salt `final:<specification key>:<sha256 of the recorded
//!   inputs>`: output files `manifest.json` (every entry of the tree with
//!   the modes the rule left), `recorded_inputs` and `runtime_inputs`, and
//!   the output directory `repo_contents` (the tree's `Tree`, so caches
//!   that check or age the blobs an entry references keep them).

use crate::reapi::{self, Digest};
use crate::remote_cache::{BlobSource, CacheError, RemoteCache};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Salts every key: change it whenever entries written before may no
/// longer be valid (RemoteRepoContentsCacheImpl.GUID's role).
const GUID: &str = "bazel-rs-remote-repo-contents-v2-3c4f8a52-1d7e-4b9a-9e2f-6a0d5c8b7e41";

/// The command's argument: [`GUID`] with the repository store's tree profile
/// and manifest format, so bumping either (published trees may differ for
/// the same specification) re-keys every entry.
fn salt_prefix() -> String {
    format!(
        "{GUID}/{}/{}",
        crate::repository_store::TREE_PROFILE,
        crate::repository_store::TREE_MANIFEST_FORMAT
    )
}
const MANIFEST: &str = "manifest.json";
const RECORDED: &str = "recorded_inputs";
const RUNTIME: &str = "runtime_inputs";
const CONTENTS: &str = "repo_contents";
/// The manifest format this binary writes and reads.
const FORMAT: u32 = 1;
/// The final entries an intermediate entry lists (newest first).
const MAX_CANDIDATES: usize = 8;

static REQUESTED: AtomicBool = AtomicBool::new(false);

/// Transfer counters (`BAZEL_RS_REMOTE_REPO_CACHE_STATS=1` prints them at
/// exit): hits, uploads and bytes.
pub static HITS: AtomicU64 = AtomicU64::new(0);
pub static DOWNLOADED_BYTES: AtomicU64 = AtomicU64::new(0);
pub static UPLOADS: AtomicU64 = AtomicU64::new(0);
pub static UPLOADED_BYTES: AtomicU64 = AtomicU64::new(0);
/// Per-repository misses (`BAZEL_RS_REMOTE_REPO_CACHE_STATS=verbose`).
static MISSES: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// Whether the statistics list each miss (`BAZEL_RS_REMOTE_REPO_CACHE_STATS=verbose`).
pub fn verbose() -> bool {
    // snapshot-unrecorded: a diagnostics switch; no result depends on it.
    std::env::var_os("BAZEL_RS_REMOTE_REPO_CACHE_STATS").is_some_and(|value| value == "verbose")
}

/// Notes why `canonical` was not served (`verbose` statistics only).
pub fn note_miss(canonical: &str, reason: &str) {
    if verbose() {
        MISSES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((canonical.to_owned(), reason.to_owned()));
    }
}

/// Notes why the execution of `canonical` was not uploaded (`verbose`
/// statistics only; listed as `REMOTE-REPO-CACHE not uploaded`).
pub fn note_not_uploaded(canonical: &str, reason: &str) {
    if verbose() && writing().is_some() {
        NOT_UPLOADED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((canonical.to_owned(), reason.to_owned()));
    }
}
static NOT_UPLOADED: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// `--[no]experimental_remote_repo_contents_cache` (a startup option,
/// RemoteStartupOptions; the last value wins).
pub fn set_requested(requested: bool) {
    REQUESTED.store(requested, Ordering::Relaxed);
}

/// The configured cache when the option is on.
fn cache() -> Option<&'static RemoteCache> {
    REQUESTED
        .load(Ordering::Relaxed)
        .then(crate::remote_cache::active)
        .flatten()
}

/// The cache for lookups and downloads, unless they failed too often.
fn reading() -> Option<&'static RemoteCache> {
    cache().filter(|_| !LOOKUPS.broken.load(Ordering::Relaxed))
}

/// The cache for uploads, unless they failed too often.
fn writing() -> Option<&'static RemoteCache> {
    cache().filter(|_| !UPLOADS_FAILING.broken.load(Ordering::Relaxed))
}

/// Whether a repository fetch looks the cache up (`--remote_accept_cached`
/// with a remote or disk cache, RemoteRepoContentsCacheImpl.buildContext).
pub fn lookups() -> bool {
    reading().is_some_and(RemoteCache::repo_contents_readable)
}

/// Whether a fetched eligible repository is uploaded
/// (`--remote_upload_local_results` with a remote cache).
pub fn uploads() -> bool {
    writing().is_some_and(RemoteCache::repo_contents_writable)
}

/// The repositories the cache never serves nor stores (no lookup, no
/// upload): download-dominated ones, whose fetch is a checksum-pinned
/// download of about the size of the tree, so the cache would only move
/// the same bytes from another server (docs/REPO_CONTENTS_CACHE.md
/// "Skipped repositories"). An explicit, deterministic rule on the
/// repository's rule and attributes, never a heuristic on sizes: a
/// rules_python wheel repository — `whl_archive`, or `whl_library` given a
/// `.whl` (`whl_file`, or `urls` with a `filename` ending in `.whl`, the
/// test rules_python's own `whl_library` macro applies), both defined by
/// the `rules_python` module (`@@rules_python+`). Its tree holds the
/// wheel and its extraction (twice the download; on ~/code 12 GB of 22).
/// The reason when skipped.
pub fn skipped(
    rule_identity: &str,
    attributes: &std::collections::BTreeMap<String, crate::custom_rule::AttrValue>,
) -> Option<&'static str> {
    use crate::custom_rule::AttrValue;
    let (definition, name) = rule_identity.rsplit_once('%')?;
    // rules_python's own definitions only (its canonical repository).
    let file = definition.strip_prefix("@@rules_python+//")?;
    match (file, name) {
        ("python/private/pypi:whl_archive.bzl", "whl_archive") => Some("a wheel download"),
        ("python/private/pypi:whl_library.bzl", "whl_library") => {
            let wheel_file = matches!(attributes.get("whl_file"), Some(AttrValue::Label(Some(_))));
            let wheel_url = matches!(attributes.get("urls"), Some(AttrValue::Strings(urls)) if !urls.is_empty())
                && matches!(attributes.get("filename"), Some(AttrValue::String(name)) if name.ends_with(".whl"));
            (wheel_file || wheel_url).then_some("a wheel download")
        }
        _ => None,
    }
}

thread_local! {
    /// Whether the repository materialized on this thread may use the cache
    /// ([`with_policy`]).
    static ELIGIBLE: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// Runs `f` (one repository's materialization) with `eligible` as this
/// thread's policy; the previous one is restored after (a rule demanding
/// another repository nests materializations).
pub fn with_policy<T>(eligible: bool, f: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            ELIGIBLE.with(|cell| cell.set(self.0));
        }
    }
    let _restore = Restore(ELIGIBLE.with(|cell| cell.replace(eligible)));
    f()
}

/// The policy of the repository being materialized on this thread.
pub fn eligible() -> bool {
    ELIGIBLE.with(std::cell::Cell::get)
}

/// One entry of a cached tree: a directory, a regular file (its SHA-256
/// and size name its CAS blob) or a symbolic link; `mode` is what the
/// rule left (before the store sealed the tree). `path` is relative to the
/// tree, `""` its root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    pub path: String,
    pub directory: bool,
    pub mode: u32,
    pub size: u64,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symlink: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Manifest {
    format: u32,
    entries: Vec<TreeEntry>,
    /// The pack: the regular files' contents concatenated in `entries`
    /// order, cut every [`CHUNK`] bytes, each piece a zstd frame stored as
    /// one CAS blob (one request per 16 MiB instead of one per file, and
    /// compressed: a cache over a WAN serves a repository like its archive).
    chunks: Vec<Chunk>,
}

/// One piece of a pack: the CAS digest of its zstd frame and how many
/// bytes of the concatenated contents it holds.
#[derive(Clone, Serialize, Deserialize)]
struct Chunk {
    sha256: String,
    size: u64,
    raw: u64,
}

impl Chunk {
    fn digest(&self) -> Digest {
        Digest {
            hash: self.sha256.clone(),
            size: self.size,
        }
    }
}

/// Uncompressed bytes per pack chunk (bounds the memory of a transfer).
const CHUNK: usize = 16 << 20;

/// A final entry: what the publication of the tree needs.
pub struct Candidate {
    pub recorded: Vec<u8>,
    pub inputs: Vec<u8>,
    pub entries: Vec<TreeEntry>,
    chunks: Vec<Chunk>,
    key: Digest,
}

fn command() -> reapi::Command {
    reapi::Command {
        arguments: vec![salt_prefix()],
        output_paths: vec![
            CONTENTS.to_owned(),
            MANIFEST.to_owned(),
            RECORDED.to_owned(),
            RUNTIME.to_owned(),
        ],
        ..reapi::Command::default()
    }
}

/// The encoded `Action` of `salt` and its digest (the Action Cache key):
/// the fixed command, the empty input root and `salt` (`bytes salt = 9`).
fn action(salt: &str) -> (Vec<u8>, Digest) {
    let mut out = Vec::new();
    reapi::message(&mut out, 1, &Digest::of(&command().encode()).encode());
    reapi::message(&mut out, 2, &Digest::of(&[]).encode());
    reapi::bytes_field(&mut out, 9, salt.as_bytes());
    let digest = Digest::of(&out);
    (out, digest)
}

fn intermediate_salt(spec_key: &str) -> String {
    format!("spec:{spec_key}")
}

fn final_salt(spec_key: &str, recorded_digest: &str) -> String {
    format!("final:{spec_key}:{recorded_digest}")
}

fn sha256_hex(bytes: &[u8]) -> String {
    Digest::of(bytes).hash
}

/// Failures after which lookups (or uploads) stop for the rest of the
/// command: an unreachable cache warns a few times, not once per
/// repository. Counted apart: a read-only token whose uploads are refused
/// (with the default `--remote_upload_local_results`) keeps its hits.
const MAX_FAILURES: usize = 5;

struct Failures {
    count: std::sync::atomic::AtomicUsize,
    broken: AtomicBool,
    what: &'static str,
}

static LOOKUPS: Failures = Failures {
    count: std::sync::atomic::AtomicUsize::new(0),
    broken: AtomicBool::new(false),
    what: "lookups",
};
static UPLOADS_FAILING: Failures = Failures {
    count: std::sync::atomic::AtomicUsize::new(0),
    broken: AtomicBool::new(false),
    what: "uploads",
};

impl Failures {
    fn note(&self, canonical: &str, what: &str, error: &str) {
        let count = self.count.fetch_add(1, Ordering::Relaxed) + 1;
        if count <= MAX_FAILURES {
            crate::note!("WARNING: {what} for {canonical}: {error}");
        }
        if count == MAX_FAILURES {
            self.broken.store(true, Ordering::Relaxed);
            crate::note!(
                "WARNING: The remote repo contents cache failed {MAX_FAILURES} times; its {} stop for the rest of this command",
                self.what
            );
        }
    }
}

/// A lookup or download failure: a warning, never the command's failure
/// (RepositoryFetchFunction.java:282-299); the repository is fetched
/// instead.
pub fn warn(canonical: &str, what: &str, error: &str) {
    LOOKUPS.note(canonical, what, error);
}

/// The recorded-input digests of the final entries stored for
/// `spec_key`, newest first. Empty on a miss; a failure warns.
pub fn candidates(canonical: &str, spec_key: &str) -> Vec<String> {
    let Some(cache) = reading().filter(|cache| cache.repo_contents_readable()) else {
        return Vec::new();
    };
    match intermediate_lines(cache, spec_key, true) {
        Ok(lines) => lines,
        Err(error) => {
            warn(
                canonical,
                "Remote repo contents cache lookup failed",
                &error.to_string(),
            );
            Vec::new()
        }
    }
}

/// The lines of the intermediate entry of `spec_key` (`read`: through the
/// disk cache as a lookup; else the remote cache's only, as an update).
fn intermediate_lines(
    cache: &RemoteCache,
    spec_key: &str,
    read: bool,
) -> Result<Vec<String>, CacheError> {
    let (_, key) = action(&intermediate_salt(spec_key));
    let bytes = if read {
        cache.repo_contents_result(&key)?
    } else {
        cache.remote_action_result(&key, "")?
    };
    let Some(bytes) = bytes else {
        return Ok(Vec::new());
    };
    let result = reapi::ActionResult::decode(&bytes)
        .map_err(|error| CacheError::Other(format!("Failed to parse the ActionResult: {error}")))?;
    if result.exit_code != 0
        || !result.output_files.is_empty()
        || !result.output_directories.is_empty()
        || !result.output_symlinks.is_empty()
    {
        return Err(CacheError::Other(format!(
            "unexpected intermediate action result {}",
            key.hash
        )));
    }
    let stdout = if !result.stdout_raw.is_empty() {
        result.stdout_raw
    } else if let Some(digest) = result.stdout_digest.filter(|digest| digest.size != 0) {
        match cache.repo_contents_blob(&digest, &key)? {
            Some(bytes) => bytes,
            None => return Ok(Vec::new()),
        }
    } else {
        Vec::new()
    };
    let text = String::from_utf8(stdout)
        .map_err(|_| CacheError::Other("intermediate entry is not UTF-8".into()))?;
    Ok(text
        .lines()
        .filter(|line| line.len() == 64 && line.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_owned)
        .collect())
}

/// The final entry of `spec_key` whose recorded inputs hash to
/// `recorded_digest`, with its blobs (`None`: missing, malformed or
/// incomplete; a failure warns).
pub fn fetch(canonical: &str, spec_key: &str, recorded_digest: &str) -> Option<Candidate> {
    let cache = reading()?;
    match fetch_final(cache, spec_key, recorded_digest) {
        Ok(candidate) => candidate,
        Err(error) => {
            warn(
                canonical,
                "Remote repo contents cache lookup failed",
                &error.to_string(),
            );
            None
        }
    }
}

fn fetch_final(
    cache: &RemoteCache,
    spec_key: &str,
    recorded_digest: &str,
) -> Result<Option<Candidate>, CacheError> {
    let (_, key) = action(&final_salt(spec_key, recorded_digest));
    let Some(bytes) = cache.repo_contents_result(&key)? else {
        return Ok(None);
    };
    let result = reapi::ActionResult::decode(&bytes)
        .map_err(|error| CacheError::Other(format!("Failed to parse the ActionResult: {error}")))?;
    let has = |path: &str| result.output_files.iter().any(|file| file.path == path);
    if result.exit_code != 0
        || !(has(MANIFEST) && has(RECORDED) && has(RUNTIME))
        || !result.output_directories.is_empty()
        || !result.output_symlinks.is_empty()
    {
        return Err(CacheError::Other(format!(
            "unexpected action result of a remotely cached repository {}",
            key.hash
        )));
    }
    let blob = |path: &str| -> Result<Option<Vec<u8>>, CacheError> {
        let file = result
            .output_files
            .iter()
            .find(|file| file.path == path)
            .expect("checked above");
        if !file.contents.is_empty() {
            return Ok((Digest::of(&file.contents) == file.digest).then(|| file.contents.clone()));
        }
        cache.repo_contents_blob(&file.digest, &key)
    };
    let (Some(manifest), Some(recorded), Some(inputs)) =
        (blob(MANIFEST)?, blob(RECORDED)?, blob(RUNTIME)?)
    else {
        return Ok(None);
    };
    if sha256_hex(&recorded) != recorded_digest {
        return Err(CacheError::Other(format!(
            "recorded inputs of {} do not match its key",
            key.hash
        )));
    }
    let manifest: Manifest = serde_json::from_slice(&manifest)
        .map_err(|error| CacheError::Other(format!("malformed manifest: {error}")))?;
    if manifest.format != FORMAT {
        return Ok(None);
    }
    check_entries(&manifest.entries).map_err(CacheError::Other)?;
    let content: u64 = manifest
        .entries
        .iter()
        .filter(|entry| !entry.directory && entry.symlink.is_none())
        .map(|entry| entry.size)
        .sum();
    if manifest.chunks.iter().map(|chunk| chunk.raw).sum::<u64>() != content
        || manifest
            .chunks
            .iter()
            .any(|chunk| chunk.raw == 0 || chunk.raw > CHUNK as u64)
    {
        return Err(CacheError::Other(format!(
            "the pack of {} does not hold its files",
            key.hash
        )));
    }
    Ok(Some(Candidate {
        recorded,
        inputs,
        entries: manifest.entries,
        chunks: manifest.chunks,
        key,
    }))
}

/// A relative path below a tree (`""` is the root): normal components only.
fn relative_ok(path: &str) -> bool {
    path.is_empty()
        || (!path.contains('\0')
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".." && part.len() <= 255))
}

/// Whether every link of a tree stays inside it (genuine's
/// `safeForRemoteCache`: every symlink resolves within the repository).
/// `links` maps each link's path to its target. A target is relative and
/// normalized lexically from the link's directory; it never climbs above
/// the root, and none of its intermediate components names a link of the
/// tree (a link through another link — `t -> u/../..` with `u -> ../..` —
/// would resolve where the lexical walk does not look). Its last component
/// may be a link (a chain), which is checked as a link of its own.
pub fn links_within(links: &std::collections::BTreeMap<&str, &str>) -> bool {
    links
        .iter()
        .all(|(path, target)| link_within(path, target, links))
}

fn link_within(path: &str, target: &str, links: &std::collections::BTreeMap<&str, &str>) -> bool {
    if target.is_empty() || target.starts_with('/') || target.contains('\0') {
        return false;
    }
    let mut depth: Vec<&str> = path.split('/').collect();
    depth.pop();
    let components: Vec<Component<'_>> = Path::new(target).components().collect();
    for (index, component) in components.iter().enumerate() {
        match component {
            Component::Normal(name) => {
                let Some(name) = name.to_str() else {
                    return false;
                };
                depth.push(name);
                let last = components[index + 1..]
                    .iter()
                    .all(|rest| matches!(rest, Component::CurDir));
                if !last && links.contains_key(depth.join("/").as_str()) {
                    return false;
                }
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if depth.pop().is_none() {
                    return false;
                }
            }
            Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// The entries describe a tree this binary can write: a root directory,
/// normal relative paths whose parents are listed directories, regular
/// files with a digest, links that stay inside, no special mode bits.
fn check_entries(entries: &[TreeEntry]) -> Result<(), String> {
    let directories: std::collections::HashSet<&str> = entries
        .iter()
        .filter(|entry| entry.directory)
        .map(|entry| entry.path.as_str())
        .collect();
    if !directories.contains("") {
        return Err("the cached tree has no root directory".into());
    }
    let links: std::collections::BTreeMap<&str, &str> = entries
        .iter()
        .filter_map(|entry| Some((entry.path.as_str(), entry.symlink.as_deref()?)))
        .collect();
    if !links_within(&links) {
        return Err("a cached tree link leaves the repository".into());
    }
    let mut seen = std::collections::HashSet::new();
    for entry in entries {
        if !relative_ok(&entry.path) || !seen.insert(entry.path.as_str()) {
            return Err(format!("invalid cached tree path {:?}", entry.path));
        }
        if !entry.path.is_empty() {
            let parent = entry.path.rsplit_once('/').map_or("", |(parent, _)| parent);
            if !directories.contains(parent) {
                return Err(format!("cached tree path {:?} has no parent", entry.path));
            }
        }
        if entry.mode & !0o777 != 0 {
            return Err(format!("invalid mode of cached tree path {:?}", entry.path));
        }
        match &entry.symlink {
            Some(_) if entry.directory => {
                return Err(format!("cached tree link {:?} is a directory", entry.path));
            }
            Some(_) => {}
            None if !entry.directory
                && (entry.sha256.len() != 64
                    || !entry.sha256.bytes().all(|b| b.is_ascii_hexdigit())) =>
            {
                return Err(format!("invalid digest of cached file {:?}", entry.path));
            }
            None => {}
        }
    }
    Ok(())
}

/// Writes the tree of `candidate` into the empty directory `root`: the
/// directories, every file from the pack (each chunk verified as a CAS
/// blob, each file against its digest as it is written), the links, then
/// the directories' modes (deepest first). `Ok(false)` when a chunk is
/// missing; an error for a corrupt pack or a transport or write failure.
pub fn download(candidate: &Candidate, root: &Path) -> Result<bool, String> {
    use std::os::unix::fs::PermissionsExt;
    let cache = reading().ok_or("no remote cache")?;
    for entry in candidate.entries.iter().filter(|entry| entry.directory) {
        if !entry.path.is_empty() {
            std::fs::create_dir(root.join(&entry.path))
                .map_err(|error| format!("cannot create {}: {error}", entry.path))?;
        }
    }
    let mut files = candidate
        .entries
        .iter()
        .filter(|entry| !entry.directory && entry.symlink.is_none());
    let mut writer = FileWriter::default();
    // The next chunk downloads while one is written.
    let complete = std::thread::scope(|scope| -> Result<bool, String> {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let chunks = &candidate.chunks;
        let key = &candidate.key;
        scope.spawn(move || {
            for chunk in chunks {
                let fetched = cache.repo_contents_blob(&chunk.digest(), key);
                let stop = !matches!(fetched, Ok(Some(_)));
                if sender.send(fetched).is_err() || stop {
                    return;
                }
            }
        });
        for chunk in &candidate.chunks {
            let compressed = match receiver.recv() {
                Ok(Ok(Some(bytes))) => bytes,
                Ok(Ok(None)) => return Ok(false),
                Ok(Err(error)) => return Err(error.to_string()),
                Err(_) => return Err("the pack download stopped".into()),
            };
            DOWNLOADED_BYTES.fetch_add(compressed.len() as u64, Ordering::Relaxed);
            // A cancelled speculation gives up (its coordinator runs it).
            if crate::repository_speculation::cancelled() {
                return Ok(false);
            }
            let raw = decode_chunk(&compressed, chunk.raw)
                .map_err(|error| format!("corrupt pack chunk {}: {error}", chunk.sha256))?;
            if raw.len() as u64 != chunk.raw {
                return Err(format!("corrupt pack chunk {}", chunk.sha256));
            }
            let mut bytes = raw.as_slice();
            while !bytes.is_empty() {
                if writer.remaining == 0 {
                    let entry = files.next().ok_or("the pack holds more than its files")?;
                    writer.open(root, entry)?;
                    continue;
                }
                let take = bytes.len().min(writer.remaining as usize);
                writer.write(&bytes[..take])?;
                bytes = &bytes[take..];
            }
        }
        Ok(true)
    })?;
    if !complete {
        return Ok(false);
    }
    writer.finish()?;
    // Empty files hold no pack bytes.
    for entry in files {
        writer.open(root, entry)?;
        writer.finish()?;
    }
    for entry in &candidate.entries {
        if let Some(target) = &entry.symlink {
            std::os::unix::fs::symlink(target, root.join(&entry.path))
                .map_err(|error| format!("cannot link {}: {error}", entry.path))?;
        }
    }
    let mut directories: Vec<&TreeEntry> = candidate
        .entries
        .iter()
        .filter(|entry| entry.directory)
        .collect();
    directories.sort_by(|a, b| b.path.cmp(&a.path));
    for entry in directories {
        let path = if entry.path.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&entry.path)
        };
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(entry.mode))
            .map_err(|error| format!("cannot set the mode of {}: {error}", entry.path))?;
    }
    Ok(true)
}

/// One chunk's zstd frame decoded, never past `raw` + 1 bytes: a small
/// frame may claim to decode to gigabytes (an RLE block), so the decoder
/// is bounded by what the manifest says the chunk holds (at most
/// [`CHUNK`]) and anything longer is an error.
fn decode_chunk(compressed: &[u8], raw: u64) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let decoder =
        ruzstd::decoding::StreamingDecoder::new(compressed).map_err(|error| error.to_string())?;
    let mut bytes = Vec::with_capacity(raw.min(CHUNK as u64) as usize);
    decoder
        .take(raw + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > raw {
        return Err(format!("it decodes to more than {raw} bytes"));
    }
    Ok(bytes)
}

/// The file of a pack being written: its remaining bytes and their
/// running digest.
#[derive(Default)]
struct FileWriter {
    file: Option<std::io::BufWriter<std::fs::File>>,
    path: String,
    expected: String,
    mode: u32,
    remaining: u64,
    hasher: sha2::Sha256,
}

impl FileWriter {
    fn open(&mut self, root: &Path, entry: &TreeEntry) -> Result<(), String> {
        self.finish()?;
        let file = std::fs::File::create(root.join(&entry.path))
            .map_err(|error| format!("cannot write {}: {error}", entry.path))?;
        self.file = Some(std::io::BufWriter::with_capacity(1 << 16, file));
        self.path = entry.path.clone();
        self.expected = entry.sha256.clone();
        self.mode = entry.mode;
        self.remaining = entry.size;
        self.hasher = sha2::Sha256::default();
        Ok(())
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        use sha2::Digest as _;
        use std::io::Write as _;
        self.hasher.update(bytes);
        self.remaining -= bytes.len() as u64;
        let file = self.file.as_mut().ok_or("no file is open")?;
        file.write_all(bytes)
            .map_err(|error| format!("cannot write {}: {error}", self.path))
    }

    /// Closes the open file: complete, its digest as listed, its mode set.
    fn finish(&mut self) -> Result<(), String> {
        use sha2::Digest as _;
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt;
        let Some(mut file) = self.file.take() else {
            return Ok(());
        };
        if self.remaining != 0 {
            return Err(format!("the pack ends inside {}", self.path));
        }
        let actual = format!("{:x}", std::mem::take(&mut self.hasher).finalize());
        if actual != self.expected {
            return Err(format!(
                "{} of the pack has digest {actual}, not {}",
                self.path, self.expected
            ));
        }
        file.flush()
            .map_err(|error| format!("cannot write {}: {error}", self.path))?;
        let file = file
            .into_inner()
            .map_err(|error| format!("cannot write {}: {error}", self.path))?;
        file.set_permissions(std::fs::Permissions::from_mode(self.mode))
            .map_err(|error| format!("cannot set the mode of {}: {error}", self.path))
    }
}

/// A hit (counted for the statistics line).
pub fn note_hit() {
    HITS.fetch_add(1, Ordering::Relaxed);
}

/// Uploads the published tree `tree` of `spec_key` (`entries`: its entries
/// with the modes the rule left) with its recorded and runtime inputs,
/// then lists it first in the specification's intermediate entry. A
/// failure warns (addToCache).
pub fn upload(
    canonical: &str,
    spec_key: &str,
    tree: &Path,
    entries: Vec<TreeEntry>,
    recorded: &[u8],
    inputs: &[u8],
) {
    let Some(cache) = writing().filter(|cache| cache.repo_contents_writable()) else {
        return;
    };
    if let Err(error) = store(cache, spec_key, tree, entries, recorded, inputs) {
        UPLOADS_FAILING.note(
            canonical,
            "Failed to upload repo contents to remote cache",
            &error.to_string(),
        );
    }
}

fn store(
    cache: &RemoteCache,
    spec_key: &str,
    tree: &Path,
    entries: Vec<TreeEntry>,
    recorded: &[u8],
    inputs: &[u8],
) -> Result<(), CacheError> {
    let recorded_digest = sha256_hex(recorded);
    let (final_action, final_key) = action(&final_salt(spec_key, &recorded_digest));
    let chunks = upload_pack(cache, &final_key, tree, &entries)?;
    let command = command().encode();
    let manifest = serde_json::to_vec(&Manifest {
        format: FORMAT,
        entries,
        chunks: chunks.clone(),
    })
    .map_err(|error| CacheError::Other(error.to_string()))?;
    let (manifest_digest, recorded_blob, inputs_digest) = (
        Digest::of(&manifest),
        Digest::of(recorded),
        Digest::of(inputs),
    );
    let sources: Vec<(Digest, BlobSource<'_>)> = vec![
        (Digest::of(&command), BlobSource::Bytes(&command)),
        (final_key.clone(), BlobSource::Bytes(&final_action)),
        (manifest_digest.clone(), BlobSource::Bytes(&manifest)),
        (recorded_blob.clone(), BlobSource::Bytes(recorded)),
        (inputs_digest.clone(), BlobSource::Bytes(inputs)),
    ];
    let output = |path: String, digest: Digest| reapi::OutputFile {
        path,
        digest,
        is_executable: false,
        contents: Vec::new(),
    };
    let mut output_files = vec![
        output(MANIFEST.to_owned(), manifest_digest),
        output(RECORDED.to_owned(), recorded_blob),
        output(RUNTIME.to_owned(), inputs_digest),
    ];
    // The pack's blobs are outputs too: a cache that checks or ages what an
    // entry references keeps them with it.
    for (index, chunk) in chunks.iter().enumerate() {
        output_files.push(output(format!("{CONTENTS}/{index:06}"), chunk.digest()));
    }
    let result = reapi::ActionResult {
        output_files,
        ..reapi::ActionResult::default()
    };
    cache.repo_contents_store(&final_key, &sources, &result.encode())?;
    UPLOADS.fetch_add(1, Ordering::Relaxed);
    // The intermediate entry: this record first, then the others (a racy
    // update may drop a concurrent writer's line, as genuine's does).
    let mut lines = intermediate_lines(cache, spec_key, false).unwrap_or_default();
    if lines.first() == Some(&recorded_digest) {
        return Ok(());
    }
    lines.retain(|line| *line != recorded_digest);
    lines.insert(0, recorded_digest);
    lines.truncate(MAX_CANDIDATES);
    let stdout: Vec<u8> = lines
        .iter()
        .flat_map(|line| [line.as_bytes(), b"\n"].concat())
        .collect();
    let (intermediate_action, intermediate_key) = action(&intermediate_salt(spec_key));
    let stdout_digest = Digest::of(&stdout);
    let result = reapi::ActionResult {
        stdout_digest: Some(stdout_digest.clone()),
        ..reapi::ActionResult::default()
    };
    cache.repo_contents_store(
        &intermediate_key,
        &[
            (Digest::of(&command), BlobSource::Bytes(&command)),
            (
                intermediate_key.clone(),
                BlobSource::Bytes(&intermediate_action),
            ),
            (stdout_digest, BlobSource::Bytes(&stdout)),
        ],
        &result.encode(),
    )
}

/// Packs the regular files of `entries` below `tree` (each verified
/// against its digest as it is read) into zstd chunks of [`CHUNK`] bytes
/// and uploads each one as it is complete.
fn upload_pack(
    cache: &RemoteCache,
    key: &Digest,
    tree: &Path,
    entries: &[TreeEntry],
) -> Result<Vec<Chunk>, CacheError> {
    use sha2::Digest as _;
    use std::io::Read as _;
    let mut chunks = Vec::new();
    let mut buffer: Vec<u8> = Vec::with_capacity(CHUNK);
    let flush = |buffer: &mut Vec<u8>, chunks: &mut Vec<Chunk>| -> Result<(), CacheError> {
        if buffer.is_empty() {
            return Ok(());
        }
        let compressed = ruzstd::encoding::compress_to_vec(
            buffer.as_slice(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        let digest = Digest::of(&compressed);
        cache.repo_contents_upload(key, &[(digest.clone(), BlobSource::Bytes(&compressed))])?;
        UPLOADED_BYTES.fetch_add(compressed.len() as u64, Ordering::Relaxed);
        chunks.push(Chunk {
            sha256: digest.hash,
            size: digest.size,
            raw: buffer.len() as u64,
        });
        buffer.clear();
        Ok(())
    };
    let mut block = vec![0u8; 1 << 16];
    for entry in entries
        .iter()
        .filter(|entry| !entry.directory && entry.symlink.is_none())
    {
        let path = tree.join(&entry.path);
        // snapshot-unrecorded: uploading a published tree (each file checked
        // against its manifest digest); no result depends on the read.
        let mut file = std::fs::File::open(&path).map_err(|error| {
            CacheError::Other(format!("cannot read {}: {error}", path.display()))
        })?;
        let mut hasher = sha2::Sha256::default();
        let mut size = 0u64;
        loop {
            let read = file.read(&mut block).map_err(|error| {
                CacheError::Other(format!("cannot read {}: {error}", path.display()))
            })?;
            if read == 0 {
                break;
            }
            hasher.update(&block[..read]);
            size += read as u64;
            let mut bytes = &block[..read];
            while !bytes.is_empty() {
                let take = bytes.len().min(CHUNK - buffer.len());
                buffer.extend_from_slice(&bytes[..take]);
                bytes = &bytes[take..];
                if buffer.len() == CHUNK {
                    flush(&mut buffer, &mut chunks)?;
                }
            }
        }
        if size != entry.size || format!("{:x}", hasher.finalize()) != entry.sha256 {
            return Err(CacheError::Other(format!(
                "{} changed while it was uploaded",
                path.display()
            )));
        }
    }
    flush(&mut buffer, &mut chunks)?;
    Ok(chunks)
}

/// `BAZEL_RS_REMOTE_REPO_CACHE_STATS=1`: one stderr line with the counters.
pub fn report() {
    // snapshot-unrecorded: a diagnostics switch; no result depends on it.
    if std::env::var_os("BAZEL_RS_REMOTE_REPO_CACHE_STATS").is_none() {
        return;
    }
    eprintln!(
        "REMOTE-REPO-CACHE hits={} downloaded_bytes={} uploads={} uploaded_bytes={}",
        HITS.load(Ordering::Relaxed),
        DOWNLOADED_BYTES.load(Ordering::Relaxed),
        UPLOADS.load(Ordering::Relaxed),
        UPLOADED_BYTES.load(Ordering::Relaxed)
    );
    let mut misses = std::mem::take(&mut *MISSES.lock().unwrap_or_else(|e| e.into_inner()));
    misses.sort();
    for (canonical, reason) in misses {
        eprintln!("REMOTE-REPO-CACHE miss {canonical}: {reason}");
    }
    let mut skipped = std::mem::take(&mut *NOT_UPLOADED.lock().unwrap_or_else(|e| e.into_inner()));
    skipped.sort();
    for (canonical, reason) in skipped {
        eprintln!("REMOTE-REPO-CACHE not uploaded {canonical}: {reason}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_must_stay_inside_the_tree() {
        let one = |path: &str, target: &str| {
            links_within(&std::collections::BTreeMap::from([(path, target)]))
        };
        assert!(one("a/b", "c"));
        assert!(one("a/b", "../c"));
        assert!(one("a/b/c", "../../d"));
        assert!(!one("a/b", "../../c"));
        assert!(!one("a", "../x"));
        assert!(!one("a", "/abs"));
        assert!(!one("a", ""));
        assert!(one("a", "./b/../c"));
        // Through another link of the tree: lexically inside, resolved outside.
        let escape = std::collections::BTreeMap::from([
            ("a/b/c/d/e/u", "../../../../.."),
            ("a/b/c/d/e/t", "u/../../../../../etc/passwd"),
        ]);
        assert!(!links_within(&escape));
        // A chain (the last component a link) is each link's own check.
        let chain = std::collections::BTreeMap::from([("a/x", "y"), ("a/y", "../z")]);
        assert!(links_within(&chain));
        let through = std::collections::BTreeMap::from([("a/x", "y/f"), ("a/y", "../z")]);
        assert!(!links_within(&through));
    }

    #[test]
    fn chunks_decode_no_further_than_they_claim() {
        let raw = vec![7u8; 1 << 20];
        let frame = ruzstd::encoding::compress_to_vec(
            raw.as_slice(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        assert_eq!(decode_chunk(&frame, raw.len() as u64).unwrap(), raw);
        // A frame claiming 1 KiB that decodes to 1 MiB stops at 1 KiB + 1.
        let error = decode_chunk(&frame, 1024).unwrap_err();
        assert!(error.contains("more than 1024 bytes"), "{error}");
    }

    #[test]
    fn wheel_repositories_are_skipped() {
        use crate::custom_rule::AttrValue;
        let attrs = |pairs: Vec<(&str, AttrValue)>| {
            pairs
                .into_iter()
                .map(|(name, value)| (name.to_owned(), value))
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let none = attrs(vec![]);
        let archive = "@@rules_python+//python/private/pypi:whl_archive.bzl%whl_archive";
        assert!(skipped(archive, &none).is_some());
        let library = "@@rules_python+//python/private/pypi:whl_library.bzl%whl_library";
        let wheel = attrs(vec![
            ("urls", AttrValue::Strings(vec!["https://x/a.whl".into()])),
            (
                "filename",
                AttrValue::String("a-1.0-py3-none-any.whl".into()),
            ),
        ]);
        assert!(skipped(library, &wheel).is_some());
        let file = attrs(vec![(
            "whl_file",
            AttrValue::Label(Some("@@w//:a.whl".into())),
        )]);
        assert!(skipped(library, &file).is_some());
        let sdist = attrs(vec![
            (
                "urls",
                AttrValue::Strings(vec!["https://x/a.tar.gz".into()]),
            ),
            ("filename", AttrValue::String("a-1.0.tar.gz".into())),
        ]);
        assert!(skipped(library, &sdist).is_none());
        assert!(skipped(library, &none).is_none());
        // Same file names in another module are not rules_python's rules.
        let other = "@@other+//python/private/pypi:whl_archive.bzl%whl_archive";
        assert!(skipped(other, &none).is_none());
        let http = "@@bazel_tools//tools/build_defs/repo:http.bzl%http_archive";
        assert!(skipped(http, &wheel).is_none());
    }

    #[test]
    fn keys_are_stable_and_distinct() {
        let (_, intermediate) = action(&intermediate_salt("k"));
        let (_, last) = action(&final_salt("k", &"0".repeat(64)));
        assert_ne!(intermediate, last);
        assert_eq!(intermediate, action(&intermediate_salt("k")).1);
    }

    #[test]
    fn entries_are_checked() {
        let dir = |path: &str| TreeEntry {
            path: path.into(),
            directory: true,
            mode: 0o755,
            size: 0,
            sha256: String::new(),
            symlink: None,
        };
        let link = |path: &str, target: &str| TreeEntry {
            symlink: Some(target.into()),
            directory: false,
            mode: 0,
            ..dir(path)
        };
        assert!(check_entries(&[dir(""), dir("a"), link("a/l", "../x")]).is_ok());
        assert!(check_entries(&[dir("a")]).is_err());
        assert!(check_entries(&[dir(""), dir("a/b")]).is_err());
        assert!(check_entries(&[dir(""), dir("..")]).is_err());
        assert!(check_entries(&[dir(""), link("l", "/etc/passwd")]).is_err());
        assert!(check_entries(&[dir(""), link("l", "../out")]).is_err());
        // `u/..` is lexically inside but goes through the link `u`.
        assert!(check_entries(&[dir(""), link("u", "."), link("t", "u/../x")]).is_err());
        let file = TreeEntry {
            directory: false,
            sha256: "x".into(),
            ..dir("f")
        };
        assert!(check_entries(&[dir(""), file]).is_err());
    }
}
