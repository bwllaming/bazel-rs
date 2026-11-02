//! Bazel's `--repository_cache` (docs/REPOSITORY_CACHE.md): the download
//! cache genuine Bazel 9.3.1 shares between its output bases, in genuine's
//! on-disk layout, so that genuine Bazel and bazel-rs on one machine serve
//! each other's downloads.
//!
//! Pinned source: `/tmp/opencode/bazel-9.3.1-reference`,
//! `src/main/java/com/google/devtools/build/lib/bazel/repository/cache/`
//! (`RepositoryCache.java`, `DownloadCache.java`) and
//! `bazel/BazelRepositoryModule.java:309-420`.
//!
//! Layout (`DownloadCache`): `<path>/content_addressable/<algorithm>/<hex>/file`
//! holds a blob, `…/<hex>/id-<hex of the canonical id hashed with the same
//! algorithm>` marks a canonical id it was stored under, and writes go
//! through `…/<hex>/tmp-<random>` renamed over `file` (atomic; concurrent
//! creation tolerated).
//!
//! bazel-rs keeps its own download store (`<cache>/repositories/downloads`,
//! which the repository store, its garbage collection and its warm paths
//! rely on) as the first tier. This module is the second tier: a native
//! miss is looked up here before the network, and every verified download
//! is stored here too (only downloads are written, like genuine: a native
//! hit is not copied back). Blobs are hard-linked between the two tiers when they
//! are on one file system, so sharing costs no disk space.
//!
//! The empty value (`--repository_cache=`) disables the shared tier, like
//! genuine. Without the option there is no shared tier: genuine's default
//! (`<output_user_root>/cache/repos/v1`) belongs to one output user root,
//! which bazel-rs does not have; its native store is that default.

use crate::extension_download::{Algorithm, hex};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

/// `RepositoryCache.CAS_DIR`.
pub const CAS_DIR: &str = "content_addressable";
/// `RepositoryCache.CONTENTS_DIR`.
pub const CONTENTS_DIR: &str = "contents";
/// `DownloadCache.DEFAULT_CACHE_FILENAME`.
const FILE: &str = "file";
/// `DownloadCache.TMP_PREFIX`.
const TMP_PREFIX: &str = "tmp-";
/// `DownloadCache.ID_PREFIX`.
const ID_PREFIX: &str = "id-";

/// The repository cache options of one command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// The resolved `--repository_cache` (none: unset or empty).
    pub path: Option<PathBuf>,
    /// `--repo_env=BAZEL_HTTP_RULES_URLS_AS_DEFAULT_CANONICAL_ID=0`
    /// (`cache.bzl` `get_default_canonical_id`): http_archive's default
    /// canonical id is then empty.
    pub no_default_canonical_id: bool,
}

static SETTINGS: OnceLock<Settings> = OnceLock::new();

/// Install the command's settings (once, from main).
pub fn install(settings: Settings) {
    let _ = SETTINGS.set(settings);
}

fn settings() -> Option<&'static Settings> {
    SETTINGS.get()
}

/// The resolved `--repository_cache`, if one is in use.
pub fn path() -> Option<&'static Path> {
    settings()?.path.as_deref()
}

/// The canonical id genuine's `http_archive` downloads `urls` under
/// (`cache.bzl` `get_default_canonical_id`: the URLs joined by spaces, in
/// order): what a module's registry archive (an `http_archive` repo spec,
/// ArchiveRepoSpecBuilder) is stored under.
pub fn default_canonical_id(urls: &[String]) -> String {
    if settings().is_some_and(|settings| settings.no_default_canonical_id) {
        String::new()
    } else {
        urls.join(" ")
    }
}

/// `BazelRepositoryModule.toPath` over `OptionsUtils.PathFragmentConverter`:
/// a leading `~/` is the user's home directory, a relative path is relative
/// to the workspace, and the result is normalized like a `PathFragment`.
/// `None` for the empty value (disabled).
pub fn resolve(value: &str, workspace: &Path) -> Option<PathBuf> {
    if value.is_empty() {
        return None;
    }
    let expanded = match value.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => Path::new(&home).join(rest),
            None => PathBuf::from(value),
        },
        None => PathBuf::from(value),
    };
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        workspace.join(expanded)
    };
    Some(normalize(&joined))
}

/// Lexical normalization (`PathFragment.create`): `.` segments drop out and
/// `..` removes the previous segment.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `<root>/content_addressable/<algorithm>/<hex>`.
fn entry_at(root: &Path, algorithm: Algorithm, digest: &[u8]) -> PathBuf {
    root.join(CAS_DIR).join(algorithm.name()).join(hex(digest))
}

/// The marker file of `canonical_id` in `entry` (`DownloadCache
/// .hasCanonicalId`: the id's UTF-8 bytes hashed with the key's algorithm).
fn marker(entry: &Path, algorithm: Algorithm, canonical_id: &str) -> PathBuf {
    entry.join(format!(
        "{ID_PREFIX}{}",
        hex(&algorithm.hash(canonical_id.as_bytes()))
    ))
}

/// `DownloadCache.findCacheValue` under `root`: the entry's `file` when it
/// exists, hashes to `digest` and (for a non-empty canonical id) carries the
/// id's marker. Like genuine, the hit is touched (its last use; a read-only
/// cache is fine). A corrupt entry is a miss: genuine's `get` throws, and
/// DownloadManager downloads again (DownloadManager.java:264-277).
fn find_at(
    root: &Path,
    algorithm: Algorithm,
    digest: &[u8],
    canonical_id: &str,
) -> Option<PathBuf> {
    let entry = entry_at(root, algorithm, digest);
    let file = entry.join(FILE);
    // snapshot-unrecorded: content-addressed blob, checked against its digest below.
    if !fs::symlink_metadata(&file).is_ok_and(|metadata| metadata.is_file()) {
        return None;
    }
    if algorithm.hash_file(&file).ok()?.0 != digest {
        return None;
    }
    if !canonical_id.is_empty() && !marker(&entry, algorithm, canonical_id).exists() {
        return None;
    }
    let _ = fs::File::options()
        .write(true)
        .open(&file)
        .and_then(|handle| handle.set_modified(std::time::SystemTime::now()));
    Some(file)
}

/// Publish `source` at `destination` atomically: a hard link when allowed
/// and possible, else a copy, through a temporary file in `destination`'s
/// directory renamed over it.
fn publish(source: &Path, destination: &Path, link: bool, prefix: &str) -> io::Result<()> {
    let directory = destination
        .parent()
        .ok_or_else(|| io::Error::other("cache path without a parent"))?;
    fs::create_dir_all(directory)?;
    let temp = tempfile::Builder::new()
        .prefix(prefix)
        .tempfile_in(directory)?
        .into_temp_path();
    let linked = link && {
        fs::remove_file(&temp)?;
        fs::hard_link(source, &temp).is_ok()
    };
    if !linked {
        // A fresh file: the copy gets the default mode (umask), like
        // FileSystemUtils.copyFile into a new file.
        let mut input = fs::File::open(source)?;
        let mut output = fs::File::create(&temp)?;
        io::copy(&mut input, &mut output)?;
        output.sync_all()?;
    }
    // renameToleratingConcurrentCreation: a concurrent writer of the same
    // content may win; either file is the blob.
    temp.persist(destination).map_err(|error| error.error)?;
    Ok(())
}

/// A shared-cache hit for `digest`, materialized at `destination` (the
/// native store's blob path) without network access. `false` on a miss.
pub(crate) fn fetch_into(
    algorithm: Algorithm,
    digest: &[u8],
    canonical_id: &str,
    destination: &Path,
) -> bool {
    let Some(root) = path() else {
        return false;
    };
    fetch_into_at(root, algorithm, digest, canonical_id, destination)
}

fn fetch_into_at(
    root: &Path,
    algorithm: Algorithm,
    digest: &[u8],
    canonical_id: &str,
    destination: &Path,
) -> bool {
    let Some(file) = find_at(root, algorithm, digest, canonical_id) else {
        return false;
    };
    // The native store is content-addressed and verified on every read, so
    // sharing the inode is safe.
    publish(&file, destination, true, ".shared-").is_ok()
        && algorithm
            .hash_file(destination)
            .is_ok_and(|(actual, _)| actual == digest)
}

/// A shared-cache hit's bytes (`DownloadCache.getBytes`: no canonical id),
/// for the registry files Bzlmod reads by their recorded SHA-256.
pub(crate) fn get_bytes(algorithm: Algorithm, digest: &[u8]) -> Option<Vec<u8>> {
    let file = find_at(path()?, algorithm, digest, "")?;
    // snapshot-unrecorded: content-addressed blob, verified below.
    let bytes = fs::read(file).ok()?;
    (algorithm.hash(&bytes) == digest).then_some(bytes)
}

/// `DownloadCache.put` after a verified download: `source` (whose digest
/// is `digest`) enters the shared cache, with the canonical id's marker.
/// An existing intact `file` is kept (content-addressed), the marker is
/// added either way. Errors are the caller's (genuine: an IOException of
/// the download).
pub(crate) fn put(
    algorithm: Algorithm,
    digest: &[u8],
    canonical_id: &str,
    source: &Path,
) -> io::Result<()> {
    let Some(root) = path() else {
        return Ok(());
    };
    put_at(root, algorithm, digest, canonical_id, source)
}

fn put_at(
    root: &Path,
    algorithm: Algorithm,
    digest: &[u8],
    canonical_id: &str,
    source: &Path,
) -> io::Result<()> {
    let entry = entry_at(root, algorithm, digest);
    let file = entry.join(FILE);
    let intact = fs::symlink_metadata(&file).is_ok_and(|metadata| metadata.is_file())
        && algorithm
            .hash_file(&file)
            .is_ok_and(|(actual, _)| actual == digest);
    if !intact {
        publish(source, &file, true, TMP_PREFIX)?;
        // Genuine's entries are readable by everyone sharing the cache: a
        // hard-linked native blob (created 0600) gets the default mode.
        let _ = fs::set_permissions(&file, std::os::unix::fs::PermissionsExt::from_mode(0o644));
    }
    if !canonical_id.is_empty() {
        fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(marker(&entry, algorithm, canonical_id))?;
    }
    Ok(())
}

/// A native-store hit the shared cache lacks enters it (cheaply: only the
/// entry's existence is checked, never its digest), so that genuine Bazel
/// finds what bazel-rs downloaded before `--repository_cache` was set.
pub(crate) fn offer(algorithm: Algorithm, digest: &[u8], canonical_id: &str, blob: &Path) {
    let Some(root) = path() else {
        return;
    };
    let entry = entry_at(root, algorithm, digest);
    let present = entry.join(FILE).is_file()
        && (canonical_id.is_empty() || marker(&entry, algorithm, canonical_id).exists());
    if !present {
        let _ = put_at(root, algorithm, digest, canonical_id, blob);
    }
}

/// [`put`] of in-memory bytes (`DownloadCache.put(byte[], KeyType)`).
pub(crate) fn put_bytes(algorithm: Algorithm, digest: &[u8], bytes: &[u8]) -> io::Result<()> {
    let Some(root) = path() else {
        return Ok(());
    };
    let entry = entry_at(root, algorithm, digest);
    let file = entry.join(FILE);
    if fs::symlink_metadata(&file).is_ok_and(|metadata| metadata.is_file())
        && fs::read(&file).is_ok_and(|existing| existing == bytes)
    {
        return Ok(());
    }
    fs::create_dir_all(&entry)?;
    let mut temp = tempfile::Builder::new()
        .prefix(TMP_PREFIX)
        .tempfile_in(&entry)?;
    io::Write::write_all(temp.as_file_mut(), bytes)?;
    temp.as_file().sync_all()?;
    let _ = fs::set_permissions(
        temp.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o644),
    );
    temp.persist(&file).map_err(|error| error.error)?;
    Ok(())
}

/// Copy a cache hit to a download's destination (`DownloadCache.get`). The
/// destination must not exist. `--experimental_repository_cache_hardlinks`
/// (a hard link instead, to save disk space) is accepted without effect:
/// bazel-rs publishes repositories as immutable trees that never share an
/// inode with a cache entry (a repository rule may rewrite its files), and
/// the copy's contents are the same.
pub(crate) fn materialize_hit(blob: &Path, target: &Path) -> io::Result<()> {
    let mut source = fs::File::open(blob)?;
    let mut destination = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)?;
    io::copy(&mut source, &mut destination)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_download_cache() {
        let temp = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let root = temp.path().join("repo");
        let source = temp.path().join("blob");
        fs::write(&source, b"hello").unwrap();
        let digest = Algorithm::Sha256.hash(b"hello");
        put_at(&root, Algorithm::Sha256, &digest, "u1 u2", &source).unwrap();
        let entry = root.join("content_addressable/sha256").join(hex(&digest));
        assert_eq!(fs::read(entry.join("file")).unwrap(), b"hello");
        // sha256("u1 u2")
        assert!(
            entry
                .join(format!(
                    "id-{}",
                    crate::extension_download::sha256(b"u1 u2")
                ))
                .exists()
        );
        let out = temp.path().join("native/blob");
        assert!(fetch_into_at(
            &root,
            Algorithm::Sha256,
            &digest,
            "u1 u2",
            &out
        ));
        assert_eq!(fs::read(&out).unwrap(), b"hello");
        // Another canonical id is a miss, no canonical id a hit.
        assert!(find_at(&root, Algorithm::Sha256, &digest, "other").is_none());
        assert!(find_at(&root, Algorithm::Sha256, &digest, "").is_some());
        // A corrupt entry is a miss; a put repairs it.
        fs::remove_file(entry.join("file")).unwrap();
        fs::write(entry.join("file"), b"tampered").unwrap();
        assert!(find_at(&root, Algorithm::Sha256, &digest, "").is_none());
        put_at(&root, Algorithm::Sha256, &digest, "", &source).unwrap();
        assert!(find_at(&root, Algorithm::Sha256, &digest, "").is_some());
        // No temporary files remain.
        let names: Vec<_> = fs::read_dir(&entry)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert!(names.iter().all(|n| !n.starts_with("tmp-")), "{names:?}");
    }

    #[test]
    fn paths_resolve_like_path_fragment_converter() {
        let ws = Path::new("/ws");
        assert_eq!(resolve("", ws), None);
        assert_eq!(resolve("/a/./b/../c", ws), Some(PathBuf::from("/a/c")));
        assert_eq!(resolve("cache", ws), Some(PathBuf::from("/ws/cache")));
    }
}
