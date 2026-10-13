//! Commit-pinned Git sources, not general git_repository rule execution.
//!
//! Only a private, disposable checkout is used. The published tree contains no
//! Git database. Like the archive materializer, cache ancestors and the cache
//! itself must be trusted against concurrent hostile filesystem replacement.
//! Process isolation is not a sandbox against vulnerabilities in Git itself.
use crate::git_process::GitRunner;
use crate::patch_inputs::LocalPatchPlan;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

const POLICY: &str = "native-git-source-v1";
const MAX_ENTRIES: usize = 100_000;
const MAX_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_DEPTH: usize = 64;
const MODULE_LIMIT: u64 = 16 * 1024 * 1024;
const LISTING_LIMIT: usize = 1024 * 1024;
const MANIFEST_LIMIT: u64 = 64 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitSpec {
    pub remote: String,
    pub commit: String,
    pub strip_prefix: String,
}

impl GitSpec {
    /// Callers reject tag/branch/submodule and other unsupported options before
    /// constructing this deliberately narrow specification. Local file URLs are
    /// an explicit fixture transport, never an implicit local-path fallback.
    pub fn validate(&self, allow_remote: bool) -> Result<(), String> {
        if !full_sha(&self.commit) {
            return Err(
                "Git commit must be a full 40-hex SHA-1, not a ref or abbreviated hash".into(),
            );
        }
        if !self.strip_prefix.is_empty() {
            safe_relative(&self.strip_prefix)?;
        }
        let remote = &self.remote;
        if remote.chars().any(|c| c.is_control() || c.is_whitespace())
            || remote.contains(['\\', '?', '#'])
        {
            return Err("unsafe Git remote URL".into());
        }
        let url = reqwest::Url::parse(remote).map_err(|_| "invalid Git remote URL")?;
        // Check the raw authority as well: URL parsers can discard empty
        // userinfo and normalize localhost or malformed slash spellings.
        let (scheme, rest) = remote
            .split_once("://")
            .ok_or("Git remote requires an explicit URL")?;
        let authority = rest.split('/').next().unwrap_or("");
        if authority.contains('@')
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || contains_encoded_control(remote)
        {
            return Err(
                "Git remote must not contain credentials, query, fragment or control bytes".into(),
            );
        }
        match scheme {
            "https" if !authority.is_empty() && url.host_str().is_some() => {
                if !allow_remote {
                    return Err("remote Git sources require --lockfile_mode=off".into());
                }
                Ok(())
            }
            "file" if authority.is_empty() && rest.starts_with('/') && !rest.starts_with("//") => {
                let path = url
                    .to_file_path()
                    .map_err(|_| "invalid local Git file URL")?;
                let text = path.to_str().ok_or("non-UTF-8 local Git path")?;
                if !path.is_absolute()
                    || text.contains('\\')
                    || text.chars().any(char::is_control)
                    || url.host_str().is_some()
                {
                    return Err(
                        "Git file URL must name an absolute local path without a host".into(),
                    );
                }
                Ok(())
            }
            _ => Err(
                "Git remote must be HTTPS or an explicit hostless absolute file:// fixture URL"
                    .into(),
            ),
        }
    }
}

fn contains_encoded_control(text: &str) -> bool {
    text.as_bytes().windows(3).any(|w| {
        w[0] == b'%'
            && std::str::from_utf8(&w[1..])
                .ok()
                .and_then(|s| u8::from_str_radix(s, 16).ok())
                .is_some_and(|b| b < 32 || b == 127 || b == b'\\')
    })
}

fn full_sha(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitTree {
    pub root: PathBuf,
    pub commit: String,
    pub module_file: Vec<u8>,
}

/// The graph validates final MODULE syntax, name and dependencies through this
/// callback, before publication and on every verified warm read. It must treat
/// the supplied tree as read-only; its staging path is not a persistent handle.
pub fn materialize<T>(
    spec: &GitSpec,
    patches: &LocalPatchPlan,
    cache: &Path,
    allow_remote: bool,
    validate: impl Fn(&Path, &[u8]) -> Result<T, String>,
) -> Result<(GitTree, T), String> {
    spec.validate(allow_remote)?;
    patches.validate_materialization()?;
    let commit = spec.commit.to_ascii_lowercase();
    let identity = serde_json::to_vec(&(
        POLICY,
        (&spec.remote, &commit, &spec.strip_prefix),
        patches.cache_identity(),
    ))
    .map_err(|e| e.to_string())?;
    let key = format!("{:x}", Sha256::digest(identity));
    ensure_directory(cache)?;
    let cache = fs::canonicalize(cache)
        .map_err(|e| e.to_string())?
        .join("git");
    ensure_directory(&cache)?;
    let trees = cache.join("trees");
    let manifests = cache.join("manifests");
    let locks = cache.join("locks");
    for directory in [&trees, &manifests, &locks] {
        ensure_directory(directory)?;
    }
    let lock_path = locks.join(&key);
    check_optional_regular(&lock_path)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("cannot open Git cache lock: {e}"))?;
    lock.lock_exclusive()
        .map_err(|e| format!("cannot lock Git cache: {e}"))?;
    // File owns the advisory lock, released on every return/error via Drop.
    let destination = trees.join(&key);
    let manifest_path = manifests.join(format!("{key}.json"));
    check_optional_directory(&destination)?;
    check_optional_regular(&manifest_path)?;
    if destination.is_dir()
        && let Ok(bytes) = read_bounded(&manifest_path, MANIFEST_LIMIT)
        && let Ok(manifest) = serde_json::from_slice::<Manifest>(&bytes)
        && manifest.policy == POLICY
        && manifest.key == key
        && manifest.commit == commit
        && snapshot(&destination, false).is_ok_and(|actual| actual == manifest.files)
    {
        let module_file = read_bounded(&destination.join("MODULE.bazel"), MODULE_LIMIT)?;
        let value = validate(&destination, &module_file)?;
        return Ok((
            GitTree {
                root: destination,
                commit,
                module_file,
            },
            value,
        ));
    }

    let stage = tempfile::Builder::new()
        .prefix(".stage-")
        .tempdir_in(&trees)
        .map_err(|e| format!("cannot stage Git tree: {e}"))?;
    fetch_export(spec, &commit, &cache, stage.path())?;
    patches.apply_tree(stage.path())?;
    let files = snapshot(stage.path(), true)?;
    let module_file = read_bounded(&stage.path().join("MODULE.bazel"), MODULE_LIMIT)?;
    let value = validate(stage.path(), &module_file)?;
    let manifest = Manifest {
        policy: POLICY.into(),
        key,
        commit: commit.clone(),
        files,
    };
    let encoded = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
    if encoded.len() as u64 > MANIFEST_LIMIT {
        return Err("Git manifest exceeds size limit".into());
    }
    // Only remove our known-key corrupt tree after its replacement is complete
    // and validated. Never remove arbitrary paths or non-directory destinations.
    check_optional_directory(&destination)?;
    check_optional_regular(&manifest_path)?;
    if destination.exists() {
        fs::remove_dir_all(&destination)
            .map_err(|e| format!("cannot remove corrupt Git tree: {e}"))?;
    }
    fs::rename(stage.path(), &destination).map_err(|e| format!("cannot publish Git tree: {e}"))?;
    // These are two atomic renames, NOT an atomic pair. Missing/mismatching
    // manifests after a crash trigger refetch, never silent source reuse.
    if let Err(error) = atomic_write(&manifest_path, &encoded) {
        fs::remove_dir_all(&destination)
            .map_err(|cleanup| format!("{error}; cannot clean unpublished Git tree: {cleanup}"))?;
        return Err(error);
    }
    Ok((
        GitTree {
            root: destination,
            commit,
            module_file,
        },
        value,
    ))
}

fn remaining(start: Instant) -> Result<Duration, String> {
    DEADLINE
        .checked_sub(start.elapsed())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| "Git materialization exceeded its 120-second command deadline".into())
}

fn fetch_export(spec: &GitSpec, commit: &str, cache: &Path, stage: &Path) -> Result<(), String> {
    let start = Instant::now();
    let private = tempfile::Builder::new()
        .prefix(".fetch-")
        .tempdir_in(cache)
        .map_err(|e| format!("cannot stage private Git work: {e}"))?;
    let isolation = private.path().join("isolation");
    fs::create_dir(&isolation).map_err(|e| e.to_string())?;
    let runner = GitRunner::new(&isolation, spec.remote.starts_with("file://"))?;
    let mut first_error = None;
    // Retry once without shallow history, in a fresh private repository. Never
    // reuse a failed fetch's database, refs, or partially checked out files.
    for shallow in [true, false] {
        let checkout = tempfile::Builder::new()
            .prefix("checkout-")
            .tempdir_in(private.path())
            .map_err(|e| e.to_string())?;
        let run = |args: &[&str]| runner.run(checkout.path(), args, remaining(start)?);
        run(&["init", "--quiet"])?;
        run(&["remote", "add", "origin", &spec.remote])?;
        let fetch = if shallow {
            run(&["fetch", "--no-tags", "--depth=1", "origin", commit])
        } else {
            run(&["fetch", "--no-tags", "origin", commit])
        };
        if let Err(error) = fetch {
            if shallow {
                first_error = Some(error);
                continue;
            }
            return Err(format!(
                "Git fetch failed (shallow: {}; full: {error})",
                first_error.unwrap_or_default()
            ));
        }
        let resolved = run(&[
            "rev-parse",
            "--verify",
            "--end-of-options",
            "FETCH_HEAD^{commit}",
        ])?;
        verify_commit(&resolved.stdout, commit)?;
        let listing = run(&["ls-tree", "-rz", "--full-tree", commit])?;
        let entries = parse_listing(&listing.stdout)?;
        // Validate every tracked leaf, including those outside strip_prefix,
        // before checkout can create a symlink or encounter a gitlink.
        run(&[
            "-c",
            "core.autocrlf=false",
            "checkout",
            "--detach",
            "--force",
            commit,
        ])?;
        let head = run(&["rev-parse", "--verify", "--end-of-options", "HEAD^{commit}"])?;
        verify_commit(&head.stdout, commit)?;
        export(checkout.path(), stage, &spec.strip_prefix, &entries)?;
        return Ok(());
    }
    unreachable!("two fetch attempts always return")
}

fn verify_commit(bytes: &[u8], requested: &str) -> Result<(), String> {
    let actual = std::str::from_utf8(bytes)
        .map_err(|_| "Git returned a non-UTF-8 commit")?
        .trim();
    if !full_sha(actual) || !actual.eq_ignore_ascii_case(requested) {
        return Err("Git resolved commit does not match the requested full SHA-1".into());
    }
    Ok(())
}

#[derive(Debug)]
struct Tracked {
    path: String,
    executable: bool,
}

fn parse_listing(bytes: &[u8]) -> Result<Vec<Tracked>, String> {
    // Reject saturation too, so a bounded runner cannot turn a truncated
    // response ending exactly on a NUL boundary into an apparently full tree.
    if bytes.len() >= LISTING_LIMIT {
        return Err("Git ls-tree listing reaches the 1 MiB limit".into());
    }
    if !bytes.is_empty() && bytes.last() != Some(&0) {
        return Err("truncated Git ls-tree listing".into());
    }
    let mut entries = Vec::new();
    let mut names = BTreeSet::new();
    if bytes.is_empty() {
        return Ok(entries);
    }
    for record in bytes[..bytes.len() - 1].split(|b| *b == 0) {
        let text = std::str::from_utf8(record).map_err(|_| "non-UTF-8 Git path unsupported")?;
        let (header, path) = text
            .split_once('\t')
            .ok_or("malformed Git ls-tree record")?;
        let fields: Vec<_> = header.split(' ').collect();
        if fields.len() != 3 || fields[1] != "blob" || !full_sha(fields[2]) {
            return Err("unsupported Git tree entry: expected regular blob (no gitlinks)".into());
        }
        let executable = match fields[0] {
            "100644" => false,
            "100755" => true,
            _ => {
                return Err(
                    "unsupported Git tree mode: symlinks and special entries are forbidden".into(),
                );
            }
        };
        safe_relative(path)?;
        if !names.insert(path.to_owned()) {
            return Err("duplicate Git tree path".into());
        }
        entries.push(Tracked {
            path: path.into(),
            executable,
        });
        if entries.len() > MAX_ENTRIES {
            return Err("Git tree exceeds 100000 entries".into());
        }
    }
    Ok(entries)
}

fn safe_relative(text: &str) -> Result<(), String> {
    if text.is_empty()
        || text.contains(['\\', ':', '?', '#', '%'])
        || text.chars().any(char::is_control)
        || text
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == ".." || p.eq_ignore_ascii_case(".git"))
        || Path::new(text)
            .components()
            .any(|p| !matches!(p, Component::Normal(_)))
        || text.split('/').count() > MAX_DEPTH
    {
        return Err(format!(
            "unsafe Git relative path or strip_prefix {text:?} (maximum depth 64)"
        ));
    }
    Ok(())
}

fn export(checkout: &Path, stage: &Path, prefix: &str, entries: &[Tracked]) -> Result<(), String> {
    let source_root = checkout.join(prefix);
    check_descendant_directories(checkout, Path::new(prefix))?;
    let mut selected = 0;
    let mut total = 0u64;
    for entry in entries {
        let relative = if prefix.is_empty() {
            entry.path.as_str()
        } else if let Some(rest) = entry
            .path
            .strip_prefix(prefix)
            .and_then(|s| s.strip_prefix('/'))
        {
            rest
        } else {
            continue;
        };
        let source = source_root.join(relative);
        check_descendant_directories(
            &source_root,
            Path::new(relative).parent().unwrap_or(Path::new("")),
        )?;
        if regular_metadata(&source)?.len() > MAX_BYTES - total {
            return Err("Git export exceeds 1 GiB total file bytes".into());
        }
        let target = stage.join(relative);
        ensure_directory(target.parent().ok_or("Git export path has no parent")?)?;
        let mut input = File::open(&source)
            .map_err(|e| e.to_string())?
            .take(MAX_BYTES - total + 1);
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&target)
            .map_err(|e| e.to_string())?;
        total += std::io::copy(&mut input, &mut output).map_err(|e| e.to_string())?;
        if total > MAX_BYTES {
            return Err("Git export exceeds 1 GiB total file bytes".into());
        }
        set_mode(&target, if entry.executable { 0o755 } else { 0o644 })?;
        selected += 1;
    }
    if !prefix.is_empty() && selected == 0 {
        return Err("Git strip_prefix has no tracked descendant files".into());
    }
    Ok(())
}

fn check_descendant_directories(root: &Path, relative: &Path) -> Result<(), String> {
    let mut path = root.to_path_buf();
    required_directory(&path)?;
    for component in relative.components() {
        path.push(component);
        required_directory(&path)?;
    }
    Ok(())
}

fn required_directory(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| format!("cannot inspect Git directory {}: {e}", path.display()))?;
    if !metadata.is_dir() {
        return Err(format!(
            "Git path is not a regular directory (symlinks forbidden): {}",
            path.display()
        ));
    }
    Ok(())
}

fn optional_metadata(path: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "symlink Git cache path is unsupported: {}",
            path.display()
        )),
        Ok(metadata) => Ok(Some(metadata)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot inspect Git cache {}: {e}", path.display())),
    }
}

fn check_optional_directory(path: &Path) -> Result<(), String> {
    if optional_metadata(path)?.is_some_and(|m| !m.is_dir()) {
        return Err(format!(
            "Git cache destination is not a directory: {}",
            path.display()
        ));
    }
    Ok(())
}

fn check_optional_regular(path: &Path) -> Result<(), String> {
    if optional_metadata(path)?.is_some() {
        regular_metadata(path)?;
    }
    Ok(())
}

fn ensure_directory(path: &Path) -> Result<(), String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err("Git cache paths containing '..' are unsupported".into());
        }
        current.push(component);
        check_optional_directory(&current)?;
        match fs::create_dir(&current) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                required_directory(&current)?
            }
            Err(e) => {
                return Err(format!(
                    "cannot create Git directory {}: {e}",
                    current.display()
                ));
            }
        }
    }
    Ok(())
}

fn regular_metadata(path: &Path) -> Result<fs::Metadata, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|e| format!("cannot inspect Git file {}: {e}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "Git file is not regular (symlinks/special files forbidden): {}",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(format!(
                "hardlinked Git file is unsupported: {}",
                path.display()
            ));
        }
    }
    Ok(metadata)
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, String> {
    if regular_metadata(path)?.len() > limit {
        return Err(format!(
            "Git file exceeds {limit}-byte limit: {}",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|e| e.to_string())?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > limit {
        return Err(format!(
            "Git file exceeds {limit}-byte limit: {}",
            path.display()
        ));
    }
    Ok(bytes)
}

fn mode(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        if metadata.is_dir() { 0o755 } else { 0o644 }
    }
}

fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    policy: String,
    key: String,
    commit: String,
    files: BTreeMap<String, Entry>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    directory: bool,
    mode: u32,
    size: u64,
    sha256: String,
}

fn snapshot(root: &Path, normalize: bool) -> Result<BTreeMap<String, Entry>, String> {
    fn visit(
        root: &Path,
        path: &Path,
        normalize: bool,
        files: &mut BTreeMap<String, Entry>,
        total: &mut u64,
    ) -> Result<(), String> {
        let metadata = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        let name = path
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .to_str()
            .ok_or("non-UTF-8 Git tree path unsupported")?
            .to_owned();
        if !name.is_empty() {
            safe_relative(&name)?;
        }
        if files.len() > MAX_ENTRIES {
            return Err("Git snapshot exceeds 100000 entries".into());
        }
        if !metadata.is_dir() {
            regular_metadata(path)?;
        }
        let mut entry = Entry {
            directory: metadata.is_dir(),
            mode: mode(&metadata),
            size: 0,
            sha256: String::new(),
        };
        if normalize {
            entry.mode = if entry.directory || entry.mode & 0o100 != 0 {
                0o755
            } else {
                0o644
            };
            set_mode(path, entry.mode)?;
        }
        if !entry.directory {
            if metadata.len() > MAX_BYTES - *total {
                return Err("Git snapshot exceeds 1 GiB total file bytes".into());
            }
            let mut digest = Sha256::new();
            let mut input = File::open(path)
                .map_err(|e| e.to_string())?
                .take(MAX_BYTES - *total + 1);
            entry.size = std::io::copy(&mut input, &mut digest).map_err(|e| e.to_string())?;
            *total += entry.size;
            if *total > MAX_BYTES {
                return Err("Git snapshot exceeds 1 GiB total file bytes".into());
            }
            entry.sha256 = format!("{:x}", digest.finalize());
        }
        let directory = entry.directory;
        files.insert(name, entry);
        if directory {
            for child in fs::read_dir(path).map_err(|e| e.to_string())? {
                visit(
                    root,
                    &child.map_err(|e| e.to_string())?.path(),
                    normalize,
                    files,
                    total,
                )?;
            }
        }
        Ok(())
    }
    required_directory(root)?;
    let mut files = BTreeMap::new();
    visit(root, root, normalize, &mut files, &mut 0)?;
    Ok(files)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    check_optional_regular(path)?;
    let parent = path.parent().ok_or("Git manifest has no parent")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    temp.write_all(bytes)
        .and_then(|()| temp.as_file().sync_all())
        .map_err(|e| e.to_string())?;
    temp.persist(path)
        .map_err(|e| format!("cannot publish Git manifest: {e}"))?;
    File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bzlmod::{ModuleFile, PatchOptions};
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const MODULE: &[u8] = b"module(name=\"dep\",version=\"arbitrary\")\n";

    struct Fixture {
        temp: tempfile::TempDir,
        origin: PathBuf,
        home: PathBuf,
        cache: PathBuf,
        spec: GitSpec,
    }

    impl Fixture {
        fn new(prefix: &str) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let origin = temp.path().join("origin");
            let home = temp.path().join("fixture-home");
            fs::create_dir(&origin).unwrap();
            fs::create_dir(&home).unwrap();
            let mut fixture = Self {
                cache: temp.path().join("cache"),
                spec: GitSpec {
                    remote: reqwest::Url::from_file_path(&origin).unwrap().to_string(),
                    commit: String::new(),
                    strip_prefix: prefix.into(),
                },
                temp,
                origin,
                home,
            };
            fixture.git(&["init", "--quiet"]);
            let source = fixture.origin.join(prefix);
            fs::create_dir_all(&source).unwrap();
            fs::write(source.join("MODULE.bazel"), MODULE).unwrap();
            fs::write(source.join("data"), b"original\n").unwrap();
            fs::write(source.join("run"), b"#!/bin/sh\nexit 0\n").unwrap();
            set_mode(&source.join("run"), 0o755).unwrap();
            if !prefix.is_empty() {
                fs::write(fixture.origin.join("outside"), b"not exported\n").unwrap();
            }
            fixture.commit();
            fixture
        }

        fn git(&self, args: &[&str]) -> Vec<u8> {
            // Fixture construction uses real Git, with no user/global config,
            // hooks or environment inherited from the application checkout.
            let output = Command::new("/usr/bin/git")
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", &self.home)
                .env("XDG_CONFIG_HOME", &self.home)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00Z")
                .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00Z")
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "init.templateDir=",
                    "-c",
                    "commit.gpgSign=false",
                    "-c",
                    "core.autocrlf=false",
                ])
                .args(args)
                .current_dir(&self.origin)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        }

        fn commit(&mut self) {
            self.git(&["add", "--all"]);
            self.commit_index();
        }

        fn commit_index(&mut self) {
            self.git(&["commit", "--quiet", "-m", "fixture"]);
            self.spec.commit = String::from_utf8(self.git(&["rev-parse", "HEAD"]))
                .unwrap()
                .trim()
                .into();
        }

        fn tree(&self) -> GitTree {
            materialize(
                &self.spec,
                &LocalPatchPlan::default(),
                &self.cache,
                true,
                |_, bytes| {
                    assert_eq!(bytes, MODULE);
                    Ok(())
                },
            )
            .unwrap()
            .0
        }

        fn assert_no_publication(&self) {
            let trees = self.cache.join("git/trees");
            assert!(!trees.exists() || fs::read_dir(trees).unwrap().next().is_none());
            self.assert_no_temporaries();
        }

        fn assert_no_temporaries(&self) {
            for dir in [self.cache.join("git"), self.cache.join("git/trees")] {
                if dir.exists() {
                    assert!(
                        fs::read_dir(dir).unwrap().all(|e| !e
                            .unwrap()
                            .file_name()
                            .to_string_lossy()
                            .starts_with('.'))
                    );
                }
            }
        }

        fn manifest(&self, tree: &GitTree) -> PathBuf {
            self.cache.join("git/manifests").join(format!(
                "{}.json",
                tree.root.file_name().unwrap().to_str().unwrap()
            ))
        }
    }

    #[test]
    fn rejects_invalid_specs_before_cache_access() {
        let temp = tempfile::tempdir().unwrap();
        let good = GitSpec {
            remote: "https://example.invalid/repo.git".into(),
            commit: "aB".repeat(20),
            strip_prefix: String::new(),
        };
        assert!(good.validate(true).is_ok());
        let mut cases = Vec::new();
        for remote in [
            "http://example.invalid/repo",
            "ssh://host/repo",
            "git://host/repo",
            "host:repo",
            "/tmp/repo",
            "https://user@host/repo",
            "https://@host/repo",
            "https://user:pass@host/repo",
            "https://host/repo?x",
            "https://host/repo#x",
            "https://host/\nrepo",
            "https://host\\repo",
            "https:///host/repo",
            "https://host/%00repo",
            "file://localhost/tmp/repo",
            "file://host/tmp/repo",
            "file://user@/tmp/repo",
            "file:relative",
            "file:////tmp/repo",
            "file:///tmp/%0arepo",
        ] {
            let mut spec = good.clone();
            spec.remote = remote.into();
            cases.push(spec);
        }
        for commit in [
            "main",
            "refs/heads/main",
            "abcdef",
            "",
            &"g".repeat(40),
            &"0".repeat(41),
        ] {
            let mut spec = good.clone();
            spec.commit = commit.into();
            cases.push(spec);
        }
        for prefix in [
            "../x", "/x", "a//b", "a/./b", "a/../b", "a/", "a\\b", "a:b", ".git", "a/.GiT/b",
            "a\0b",
        ] {
            let mut spec = good.clone();
            spec.strip_prefix = prefix.into();
            cases.push(spec);
        }
        let cache = temp.path().join("cache");
        for spec in cases {
            assert!(
                materialize(&spec, &LocalPatchPlan::default(), &cache, true, |_, _| Ok(
                    ()
                ))
                .is_err(),
                "{spec:?}"
            );
            assert!(!cache.exists());
        }
        assert!(
            materialize(&good, &LocalPatchPlan::default(), &cache, false, |_, _| Ok(
                ()
            ))
            .unwrap_err()
            .contains("--lockfile_mode=off")
        );
        assert!(!cache.exists());
    }

    #[test]
    fn explicit_local_git_does_not_require_remote_network_opt_in() {
        let fixture = Fixture::new("");
        let (tree, ()) = materialize(
            &fixture.spec,
            &LocalPatchPlan::default(),
            &fixture.cache,
            false,
            |_, bytes| {
                assert_eq!(bytes, MODULE);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(fs::read(tree.root.join("data")).unwrap(), b"original\n");
        fixture.assert_no_temporaries();
    }

    #[test]
    fn cold_export_and_offline_warm_hit_normalize_commit_and_validate_every_time() {
        let mut fixture = Fixture::new("");
        let tree = fixture.tree();
        let original_commit = fixture.spec.commit.clone();
        assert_eq!(tree.commit, original_commit);
        assert_eq!(tree.module_file, MODULE);
        assert_eq!(fs::read(tree.root.join("data")).unwrap(), b"original\n");
        assert_eq!(fixture.git(&["status", "--porcelain"]), b"");
        assert!(!tree.root.join(".git").exists());
        #[cfg(unix)]
        assert_eq!(mode(&fs::metadata(tree.root.join("run")).unwrap()), 0o755);
        fs::rename(&fixture.origin, fixture.temp.path().join("offline-origin")).unwrap();
        fixture.spec.commit.make_ascii_uppercase();
        let calls = AtomicUsize::new(0);
        let (warm, value) = materialize(
            &fixture.spec,
            &LocalPatchPlan::default(),
            &fixture.cache,
            true,
            |root, bytes| {
                calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(root, tree.root);
                assert_eq!(bytes, MODULE);
                Ok(42)
            },
        )
        .unwrap();
        assert_eq!(warm, tree);
        assert_eq!(value, 42);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            materialize(
                &fixture.spec,
                &LocalPatchPlan::default(),
                &fixture.cache,
                false,
                |_, _| Ok(())
            )
            .unwrap()
            .0,
            tree
        );
        fixture.assert_no_temporaries();
    }

    #[test]
    fn corrupt_content_extra_files_and_missing_or_invalid_manifests_refetch() {
        let fixture = Fixture::new("");
        let tree = fixture.tree();
        fs::write(tree.root.join("data"), b"corrupt").unwrap();
        fs::write(tree.root.join("extra"), b"untracked").unwrap();
        assert_eq!(fixture.tree(), tree);
        assert_eq!(fs::read(tree.root.join("data")).unwrap(), b"original\n");
        assert!(!tree.root.join("extra").exists());
        let manifest = fixture.manifest(&tree);
        fs::remove_file(&manifest).unwrap();
        assert_eq!(fixture.tree(), tree);
        fs::write(&manifest, b"not JSON").unwrap();
        assert_eq!(fixture.tree(), tree);
        // If origin is absent, missing metadata must not silently reuse a tree.
        fs::remove_file(manifest).unwrap();
        fs::rename(&fixture.origin, fixture.temp.path().join("offline-origin")).unwrap();
        assert!(
            materialize(
                &fixture.spec,
                &LocalPatchPlan::default(),
                &fixture.cache,
                true,
                |_, _| Ok(())
            )
            .is_err()
        );
        assert!(tree.root.is_dir());
        fixture.assert_no_temporaries();
    }

    #[test]
    fn strips_directory_prefix_without_exporting_parents_or_git_metadata() {
        let fixture = Fixture::new("parent/pkg");
        let tree = fixture.tree();
        assert_eq!(fs::read(tree.root.join("data")).unwrap(), b"original\n");
        for excluded in ["parent", "outside", ".git"] {
            assert!(!tree.root.join(excluded).exists());
        }
        assert!(
            snapshot(&tree.root, false)
                .unwrap()
                .keys()
                .all(|p| !p.split('/').any(|c| c.eq_ignore_ascii_case(".git")))
        );
    }

    fn change(path: &str, before: &str, after: &str) -> String {
        format!(
            "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-{before}\n+{after}\n"
        )
    }

    fn patch_plan(root: &Path, text: &str) -> LocalPatchPlan {
        fs::write(root.join("BUILD.bazel"), "").unwrap();
        fs::write(root.join("fix.patch"), text).unwrap();
        crate::patch_inputs::snapshot(
            root,
            &ModuleFile::default(),
            &PatchOptions {
                patches: vec!["//:fix.patch".into()],
                patch_strip: 1,
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn patches_module_and_data_once_before_callback_and_changes_key() {
        let fixture = Fixture::new("");
        let original = fixture.tree();
        let before = std::str::from_utf8(MODULE).unwrap().trim_end();
        let text = change(
            "MODULE.bazel",
            before,
            "module(name=\"dep\",version=\"patched\")",
        ) + &change("data", "original", "patched");
        let plan = patch_plan(fixture.temp.path(), &text);
        fs::remove_file(fixture.temp.path().join("fix.patch")).unwrap();
        let validate = |root: &Path, bytes: &[u8]| {
            assert_eq!(bytes, b"module(name=\"dep\",version=\"patched\")\n");
            assert_eq!(fs::read(root.join("data")).unwrap(), b"patched\n");
            Ok(())
        };
        let patched = materialize(&fixture.spec, &plan, &fixture.cache, true, validate)
            .unwrap()
            .0;
        assert_ne!(patched.root, original.root);
        assert_eq!(
            materialize(&fixture.spec, &plan, &fixture.cache, true, validate)
                .unwrap()
                .0,
            patched
        );
        assert_eq!(
            fs::read(fixture.origin.join("data")).unwrap(),
            b"original\n"
        );
    }

    #[test]
    fn patch_can_add_missing_module_without_module_projection() {
        let mut fixture = Fixture::new("");
        fs::remove_file(fixture.origin.join("MODULE.bazel")).unwrap();
        fixture.commit();
        let text = "diff --git a/MODULE.bazel b/MODULE.bazel\n--- /dev/null\n+++ b/MODULE.bazel\n@@ -0,0 +1 @@\n+module(name=\"dep\")\n".to_owned()
            + &change("data", "original", "added module");
        let plan = patch_plan(fixture.temp.path(), &text);
        let (tree, ()) = materialize(&fixture.spec, &plan, &fixture.cache, true, |root, bytes| {
            assert_eq!(bytes, b"module(name=\"dep\")\n");
            assert_eq!(fs::read(root.join("data")).unwrap(), b"added module\n");
            Ok(())
        })
        .unwrap();
        assert_eq!(
            materialize(&fixture.spec, &plan, &fixture.cache, true, |_, _| Ok(()))
                .unwrap()
                .0,
            tree
        );
    }

    #[test]
    fn graph_callback_rejection_prevents_publication_and_runs_on_warm_hits() {
        let fixture = Fixture::new("");
        let reject =
            |_: &Path, _: &[u8]| -> Result<(), String> { Err("module name mismatch".into()) };
        let error = materialize(
            &fixture.spec,
            &LocalPatchPlan::default(),
            &fixture.cache,
            true,
            reject,
        )
        .unwrap_err();
        assert_eq!(error, "module name mismatch");
        fixture.assert_no_publication();
        let tree = fixture.tree();
        assert_eq!(
            materialize(
                &fixture.spec,
                &LocalPatchPlan::default(),
                &fixture.cache,
                true,
                reject
            )
            .unwrap_err(),
            error
        );
        assert!(tree.root.is_dir());
    }

    #[test]
    fn missing_prefix_module_and_unknown_commit_leave_no_publication() {
        let mut fixture = Fixture::new("");
        for prefix in ["absent", "data"] {
            fixture.spec.strip_prefix = prefix.into();
            assert!(
                materialize(
                    &fixture.spec,
                    &LocalPatchPlan::default(),
                    &fixture.cache,
                    true,
                    |_, _| Ok(())
                )
                .is_err()
            );
            fixture.assert_no_publication();
        }
        fixture.spec.strip_prefix.clear();
        let original = fixture.spec.commit.clone();
        fixture.spec.commit = "0".repeat(40);
        assert!(
            materialize(
                &fixture.spec,
                &LocalPatchPlan::default(),
                &fixture.cache,
                true,
                |_, _| Ok(())
            )
            .is_err()
        );
        fixture.assert_no_publication();
        fixture.spec.commit = original;
        fs::remove_file(fixture.origin.join("MODULE.bazel")).unwrap();
        fixture.commit();
        assert!(
            materialize(
                &fixture.spec,
                &LocalPatchPlan::default(),
                &fixture.cache,
                true,
                |_, _| Ok(())
            )
            .unwrap_err()
            .contains("MODULE.bazel")
        );
        fixture.assert_no_publication();
    }

    #[test]
    fn local_command_and_external_patch_reject_before_cache_creation() {
        let fixture = Fixture::new("");
        for options in [
            PatchOptions {
                patch_cmds: vec!["touch NEVER".into()],
                ..Default::default()
            },
            PatchOptions {
                patches: vec!["@@external+//:missing.patch".into()],
                ..Default::default()
            },
        ] {
            let plan = crate::patch_inputs::snapshot(
                fixture.temp.path(),
                &ModuleFile::default(),
                &options,
            )
            .unwrap();
            assert!(
                materialize(&fixture.spec, &plan, &fixture.cache, true, |_, _| Ok(())).is_err()
            );
            assert!(!fixture.cache.exists());
        }
    }

    #[test]
    fn patch_failure_never_publishes_and_cleans_private_work() {
        let fixture = Fixture::new("");
        let plan = patch_plan(
            fixture.temp.path(),
            &change("data", "wrong context", "patched"),
        );
        assert!(materialize(&fixture.spec, &plan, &fixture.cache, true, |_, _| Ok(())).is_err());
        fixture.assert_no_publication();
    }

    #[test]
    #[cfg(unix)]
    fn rejects_tracked_symlink_even_outside_strip_prefix_before_publication() {
        use std::os::unix::fs::symlink;
        let mut fixture = Fixture::new("pkg");
        symlink("pkg/data", fixture.origin.join("link")).unwrap();
        fixture.commit();
        assert!(
            materialize(
                &fixture.spec,
                &LocalPatchPlan::default(),
                &fixture.cache,
                true,
                |_, _| Ok(())
            )
            .unwrap_err()
            .contains("symlinks")
        );
        fixture.assert_no_publication();
    }

    #[test]
    fn rejects_gitlink_even_outside_strip_prefix_before_publication() {
        let mut fixture = Fixture::new("pkg");
        fixture.git(&[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},submodule", fixture.spec.commit),
        ]);
        fixture.commit_index();
        let error = materialize(
            &fixture.spec,
            &LocalPatchPlan::default(),
            &fixture.cache,
            true,
            |_, _| Ok(()),
        )
        .unwrap_err();
        assert!(error.contains("gitlinks"), "{error}");
        fixture.assert_no_publication();
    }

    #[test]
    fn listing_parser_rejects_unsafe_modes_paths_and_truncation() {
        let sha = "a".repeat(40);
        for (mode, kind, path) in [
            ("120000", "blob", "link"),
            ("160000", "commit", "sub"),
            ("040000", "tree", "dir"),
            ("100644", "blob", "../outside"),
            ("100644", "blob", "a/.GiT/config"),
            ("100644", "blob", "/abs"),
            ("100644", "blob", "a//b"),
            ("100644", "blob", "a\\b"),
        ] {
            assert!(parse_listing(format!("{mode} {kind} {sha}\t{path}\0").as_bytes()).is_err());
        }
        let good = format!("100644 blob {sha}\t.gitattributes\0");
        assert_eq!(parse_listing(good.as_bytes()).unwrap().len(), 1);
        assert!(parse_listing(good.trim_end_matches('\0').as_bytes()).is_err());
        assert!(parse_listing((good.clone() + &good).as_bytes()).is_err());
        let mut invalid = format!("100644 blob {sha}\t").into_bytes();
        invalid.extend([255, 0]);
        assert!(parse_listing(&invalid).is_err());
        assert!(parse_listing(&vec![0; LISTING_LIMIT + 1]).is_err());
        assert!(safe_relative(&vec!["x"; 65].join("/")).is_err());
        assert!(verify_commit(format!("{}\n", "b".repeat(40)).as_bytes(), &sha).is_err());
        assert!(remaining(Instant::now() - DEADLINE).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn repairs_artifact_and_manifest_mode_corruption() {
        let fixture = Fixture::new("pkg");
        let tree = fixture.tree();
        set_mode(&tree.root.join("data"), 0o755).unwrap();
        set_mode(&tree.root, 0o700).unwrap();
        assert_eq!(fixture.tree(), tree);
        assert_eq!(mode(&fs::metadata(tree.root.join("data")).unwrap()), 0o644);
        assert_eq!(mode(&fs::metadata(&tree.root).unwrap()), 0o755);
        let path = fixture.manifest(&tree);
        let mut manifest: Manifest = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        manifest.files.get_mut("run").unwrap().mode = 0o644;
        fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        fixture.tree();
        let repaired: Manifest = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(repaired.files["run"].mode, 0o755);
    }

    #[test]
    #[cfg(unix)]
    fn refuses_cache_symlink_ancestors_and_key_paths_cold_and_warm() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new("");
        let target = fixture.temp.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = fixture.temp.path().join("link");
        symlink(&target, &link).unwrap();
        for cache in [&link, &link.join("nested")] {
            assert!(
                materialize(
                    &fixture.spec,
                    &LocalPatchPlan::default(),
                    cache,
                    true,
                    |_, _| Ok(())
                )
                .unwrap_err()
                .contains("symlink")
            );
        }
        assert!(fs::read_dir(&target).unwrap().next().is_none());
        let tree = fixture.tree();
        let manifest = fixture.manifest(&tree);
        fs::remove_file(&manifest).unwrap();
        symlink(fixture.origin.join("data"), &manifest).unwrap();
        assert!(
            materialize(
                &fixture.spec,
                &LocalPatchPlan::default(),
                &fixture.cache,
                true,
                |_, _| Ok(())
            )
            .unwrap_err()
            .contains("symlink")
        );
        fs::remove_file(&manifest).unwrap();
        fs::remove_dir_all(&tree.root).unwrap();
        symlink(&target, &tree.root).unwrap();
        assert!(
            materialize(
                &fixture.spec,
                &LocalPatchPlan::default(),
                &fixture.cache,
                true,
                |_, _| Ok(())
            )
            .unwrap_err()
            .contains("symlink")
        );
        fs::remove_file(&tree.root).unwrap();
        let lock = fixture
            .cache
            .join("git/locks")
            .join(tree.root.file_name().unwrap());
        fs::remove_file(&lock).unwrap();
        symlink(fixture.origin.join("data"), &lock).unwrap();
        assert!(
            materialize(
                &fixture.spec,
                &LocalPatchPlan::default(),
                &fixture.cache,
                true,
                |_, _| Ok(())
            )
            .unwrap_err()
            .contains("symlink")
        );
    }

    #[test]
    fn non_directory_destination_is_never_removed() {
        let fixture = Fixture::new("");
        let tree = fixture.tree();
        fs::remove_dir_all(&tree.root).unwrap();
        fs::write(&tree.root, b"keep").unwrap();
        assert!(
            materialize(
                &fixture.spec,
                &LocalPatchPlan::default(),
                &fixture.cache,
                true,
                |_, _| Ok(())
            )
            .unwrap_err()
            .contains("not a directory")
        );
        assert_eq!(fs::read(&tree.root).unwrap(), b"keep");
    }

    #[test]
    #[cfg(unix)]
    fn snapshot_rejects_hardlinks_symlinks_non_utf8_metadata_and_size_overflows() {
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(root.join("file"), "data").unwrap();
        fs::hard_link(root.join("file"), root.join("hard")).unwrap();
        assert!(snapshot(root, false).unwrap_err().contains("hardlinked"));
        fs::remove_file(root.join("hard")).unwrap();
        symlink("file", root.join("link")).unwrap();
        assert!(snapshot(root, false).is_err());
        fs::remove_file(root.join("link")).unwrap();
        let non_utf8 = root.join(std::ffi::OsString::from_vec(vec![255]));
        fs::write(&non_utf8, "data").unwrap();
        assert!(snapshot(root, false).unwrap_err().contains("non-UTF-8"));
        fs::remove_file(non_utf8).unwrap();
        fs::create_dir(root.join(".git")).unwrap();
        assert!(snapshot(root, false).is_err());
        fs::remove_dir(root.join(".git")).unwrap();
        File::create(root.join("huge"))
            .unwrap()
            .set_len(MAX_BYTES + 1)
            .unwrap();
        assert!(snapshot(root, false).unwrap_err().contains("1 GiB"));
        fs::remove_file(root.join("huge")).unwrap();
        File::create(root.join("MODULE.bazel"))
            .unwrap()
            .set_len(MODULE_LIMIT + 1)
            .unwrap();
        assert!(read_bounded(&root.join("MODULE.bazel"), MODULE_LIMIT).is_err());
    }

    #[test]
    fn concurrent_callers_share_one_verified_tree_but_all_validate() {
        let fixture = Fixture::new("");
        let calls = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        materialize(
                            &fixture.spec,
                            &LocalPatchPlan::default(),
                            &fixture.cache,
                            true,
                            |_, bytes| {
                                assert_eq!(bytes, MODULE);
                                calls.fetch_add(1, Ordering::SeqCst);
                                Ok(())
                            },
                        )
                        .unwrap()
                        .0
                    })
                })
                .collect();
            let trees: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            assert!(trees.windows(2).all(|pair| pair[0] == pair[1]));
        });
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(
            fs::read_dir(fixture.cache.join("git/trees"))
                .unwrap()
                .count(),
            1
        );
        assert_eq!(
            fs::read_dir(fixture.cache.join("git/manifests"))
                .unwrap()
                .count(),
            1
        );
        fixture.assert_no_temporaries();
    }
}
