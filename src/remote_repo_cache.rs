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
const GUID: &str = "bazel-rs-remote-repo-contents-v1-3c4f8a52-1d7e-4b9a-9e2f-6a0d5c8b7e41";
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

/// Whether a repository fetch looks the cache up (`--remote_accept_cached`
/// with a remote or disk cache, RemoteRepoContentsCacheImpl.buildContext).
pub fn lookups() -> bool {
    cache().is_some_and(RemoteCache::repo_contents_readable)
}

/// Whether a fetched eligible repository is uploaded
/// (`--remote_upload_local_results` with a remote cache).
pub fn uploads() -> bool {
    cache().is_some_and(RemoteCache::repo_contents_writable)
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
}

/// A final entry: what the publication of the tree needs.
pub struct Candidate {
    pub recorded: Vec<u8>,
    pub inputs: Vec<u8>,
    pub entries: Vec<TreeEntry>,
    key: Digest,
}

fn command() -> reapi::Command {
    reapi::Command {
        arguments: vec![GUID.to_owned()],
        output_paths: vec![
            CONTENTS.to_owned(),
            MANIFEST.to_owned(),
            RECORDED.to_owned(),
            RUNTIME.to_owned(),
        ],
        output_directories: [CONTENTS.to_owned()].into(),
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

/// A lookup or upload failure: a warning, never the command's failure
/// (RepositoryFetchFunction.java:282-299 and addToCache's warnings).
fn warn(canonical: &str, what: &str, error: &str) {
    crate::note!("WARNING: {what} for {canonical}: {error}");
}

/// The recorded-input digests of the final entries stored for
/// `spec_key`, newest first. Empty on a miss; a failure warns.
pub fn candidates(canonical: &str, spec_key: &str) -> Vec<String> {
    let Some(cache) = cache().filter(|cache| cache.repo_contents_readable()) else {
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
    let cache = cache()?;
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
    let mut paths: Vec<&str> = result
        .output_files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    paths.sort_unstable();
    if result.exit_code != 0
        || paths != [MANIFEST, RECORDED, RUNTIME]
        || result.output_directories.len() != 1
        || result.output_directories[0].path != CONTENTS
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
    Ok(Some(Candidate {
        recorded,
        inputs,
        entries: manifest.entries,
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

/// Whether the link `target` of the entry `path` stays inside the tree
/// (genuine's `safeForRemoteCache`: every symlink resolves within the
/// repository).
pub fn link_within(path: &str, target: &str) -> bool {
    if target.is_empty() || target.starts_with('/') || target.contains('\0') {
        return false;
    }
    let mut depth: Vec<&str> = path.split('/').collect();
    depth.pop();
    for component in Path::new(target).components() {
        match component {
            Component::Normal(name) => depth.push(name.to_str().unwrap_or("")),
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
            Some(target) => {
                if entry.directory || !link_within(&entry.path, target) {
                    return Err(format!(
                        "cached tree link {:?} leaves the repository",
                        entry.path
                    ));
                }
            }
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

/// Writes the tree of `candidate` into the empty directory `root`: every
/// file downloaded and verified, then links, then the directories' modes
/// (deepest first). `Ok(false)` when a blob is missing; an error for a
/// corrupt blob or a transport or write failure.
pub fn download(candidate: &Candidate, root: &Path) -> Result<bool, String> {
    use std::os::unix::fs::PermissionsExt;
    let cache = cache().ok_or("no remote cache")?;
    for entry in candidate.entries.iter().filter(|entry| entry.directory) {
        if !entry.path.is_empty() {
            std::fs::create_dir(root.join(&entry.path))
                .map_err(|error| format!("cannot create {}: {error}", entry.path))?;
        }
    }
    let files: Vec<&TreeEntry> = candidate
        .entries
        .iter()
        .filter(|entry| !entry.directory && entry.symlink.is_none())
        .collect();
    let missing = AtomicBool::new(false);
    let written = crate::remote_cache::parallel(&files, |entry| {
        if missing.load(Ordering::Relaxed) {
            return Ok(());
        }
        let digest = Digest {
            hash: entry.sha256.clone(),
            size: entry.size,
        };
        let Some(bytes) = cache.repo_contents_blob(&digest, &candidate.key)? else {
            missing.store(true, Ordering::Relaxed);
            return Ok(());
        };
        DOWNLOADED_BYTES.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        let path = root.join(&entry.path);
        std::fs::write(&path, &bytes)
            .and_then(|()| {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(entry.mode))
            })
            .map_err(|error| CacheError::Other(format!("cannot write {}: {error}", entry.path)))
    });
    written.map_err(|error| error.to_string())?;
    if missing.load(Ordering::Relaxed) {
        return Ok(false);
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
    let Some(cache) = cache().filter(|cache| cache.repo_contents_writable()) else {
        return;
    };
    if let Err(error) = store(cache, spec_key, tree, entries, recorded, inputs) {
        warn(
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
    let command = command().encode();
    let manifest = serde_json::to_vec(&Manifest {
        format: FORMAT,
        entries,
    })
    .map_err(|error| CacheError::Other(error.to_string()))?;
    let entries: Vec<TreeEntry> = serde_json::from_slice::<Manifest>(&manifest)
        .map(|manifest| manifest.entries)
        .map_err(|error| CacheError::Other(error.to_string()))?;
    let mut files = Vec::new();
    let mut symlinks = Vec::new();
    let mut directories = Vec::new();
    let mut paths = Vec::new();
    for entry in &entries {
        if entry.directory {
            if !entry.path.is_empty() {
                directories.push(entry.path.clone());
            }
        } else if let Some(target) = &entry.symlink {
            symlinks.push((entry.path.clone(), target.clone()));
        } else {
            let digest = Digest {
                hash: entry.sha256.clone(),
                size: entry.size,
            };
            files.push((entry.path.clone(), digest.clone()));
            paths.push((digest, tree.join(&entry.path)));
        }
    }
    let tree_blob = reapi::output_tree(&files, &symlinks, &directories);
    let tree_digest = Digest::of(&tree_blob);
    let (manifest_digest, recorded_blob, inputs_digest) = (
        Digest::of(&manifest),
        Digest::of(recorded),
        Digest::of(inputs),
    );
    let mut sources: Vec<(Digest, BlobSource<'_>)> = vec![
        (Digest::of(&command), BlobSource::Bytes(&command)),
        (final_key.clone(), BlobSource::Bytes(&final_action)),
        (manifest_digest.clone(), BlobSource::Bytes(&manifest)),
        (recorded_blob.clone(), BlobSource::Bytes(recorded)),
        (inputs_digest.clone(), BlobSource::Bytes(inputs)),
        (tree_digest.clone(), BlobSource::Bytes(&tree_blob)),
    ];
    let bytes: u64 = paths.iter().map(|(digest, _)| digest.size).sum();
    sources.extend(
        paths
            .iter()
            .map(|(digest, path)| (digest.clone(), BlobSource::File(path))),
    );
    let output = |path: &str, digest: Digest| reapi::OutputFile {
        path: path.to_owned(),
        digest,
        is_executable: false,
        contents: Vec::new(),
    };
    let result = reapi::ActionResult {
        output_files: vec![
            output(MANIFEST, manifest_digest),
            output(RECORDED, recorded_blob),
            output(RUNTIME, inputs_digest),
        ],
        output_directories: vec![reapi::OutputDirectory {
            path: CONTENTS.to_owned(),
            tree_digest,
            is_topologically_sorted: true,
        }],
        ..reapi::ActionResult::default()
    };
    cache.repo_contents_store(&final_key, &sources, &result.encode())?;
    UPLOADS.fetch_add(1, Ordering::Relaxed);
    UPLOADED_BYTES.fetch_add(bytes, Ordering::Relaxed);
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

/// `BAZEL_RS_REMOTE_REPO_CACHE_STATS=1`: one stderr line with the counters.
pub fn report() {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_must_stay_inside_the_tree() {
        assert!(link_within("a/b", "c"));
        assert!(link_within("a/b", "../c"));
        assert!(link_within("a/b/c", "../../d"));
        assert!(!link_within("a/b", "../../c"));
        assert!(!link_within("a", "../x"));
        assert!(!link_within("a", "/abs"));
        assert!(!link_within("a", ""));
        assert!(link_within("a", "./b/../c"));
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
        let file = TreeEntry {
            directory: false,
            sha256: "x".into(),
            ..dir("f")
        };
        assert!(check_entries(&[dir(""), file]).is_err());
    }
}
