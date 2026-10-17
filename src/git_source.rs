//! Commit-pinned Git sources, not general git_repository rule execution.
//!
//! Only a private, disposable checkout is used. The published tree contains no
//! Git database. Like the archive materializer, cache ancestors and the cache
//! itself must be trusted against concurrent hostile filesystem replacement.
//! Process isolation is not a sandbox against vulnerabilities in Git itself.
//! Native relative file symlinks preserve literal UTF-8 targets, within the
//! repository_links profile (4096-byte paths/targets, depth 64, 256 raw target
//! components, 40 hops). Directory, external and dangling links are unsupported.
use crate::git_process::GitRunner;
use crate::patch_inputs::LocalPatchPlan;
use crate::repository_links;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

// v2 trusted checkout link strings, which Git can truncate at an embedded NUL.
// Do not reuse those trees/manifests even if their filesystem snapshots match.
const POLICY: &str = "native-git-source-v3";
// Path profile of the content-addressed store (repository_links), not I/O caps.
const MAX_DEPTH: usize = 64;
const MAX_PATH_BYTES: usize = 4096;
/// Per Git command, like Bazel's git_repository, whose `ctx.execute` calls
/// use the default 600-second timeout (git_worker.bzl; repository_ctx.execute
/// `timeout` default). There are no tree size, entry-count or listing caps.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(600);

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
        && let Ok(bytes) = read_whole(&manifest_path)
        && let Ok(manifest) = serde_json::from_slice::<Manifest>(&bytes)
        && manifest.policy == POLICY
        && manifest.key == key
        && manifest.commit == commit
        && snapshot(&destination, false).is_ok_and(|actual| actual == manifest.files)
    {
        let module_file = read_whole(&destination.join("MODULE.bazel"))?;
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
    let module_file = read_whole(&stage.path().join("MODULE.bazel"))?;
    let value = validate(stage.path(), &module_file)?;
    let manifest = Manifest {
        policy: POLICY.into(),
        key,
        commit: commit.clone(),
        files,
    };
    let encoded = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
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

fn fetch_export(spec: &GitSpec, commit: &str, cache: &Path, stage: &Path) -> Result<(), String> {
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
        let run = |args: &[&str]| runner.run(checkout.path(), args, COMMAND_TIMEOUT);
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
        let expected_links = committed_links(&entries, |args| Ok(run(args)?.stdout))?;
        // Validate committed link bytes and the full namespace before checkout:
        // Git may truncate malformed link blobs when creating native symlinks.
        // Export also checks literal equality against these committed targets.
        run(&[
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.symlinks=true",
            "checkout",
            "--detach",
            "--force",
            commit,
        ])?;
        let head = run(&["rev-parse", "--verify", "--end-of-options", "HEAD^{commit}"])?;
        verify_commit(&head.stdout, commit)?;
        export(
            checkout.path(),
            stage,
            &spec.strip_prefix,
            &entries,
            &expected_links,
        )?;
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
    kind: TrackedKind,
    oid: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrackedKind {
    Regular { executable: bool },
    Symlink,
}

fn parse_listing(bytes: &[u8]) -> Result<Vec<Tracked>, String> {
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
            return Err(
                "unsupported Git tree entry: expected file or symlink blob (no gitlinks)".into(),
            );
        }
        let kind = match fields[0] {
            "100644" => TrackedKind::Regular { executable: false },
            "100755" => TrackedKind::Regular { executable: true },
            "120000" => TrackedKind::Symlink,
            _ => {
                return Err("unsupported Git tree mode: special entries are forbidden".into());
            }
        };
        safe_relative(path)?;
        if !names.insert(path.to_owned()) {
            return Err("duplicate Git tree path".into());
        }
        entries.push(Tracked {
            path: path.into(),
            kind,
            oid: fields[2].into(),
        });
    }
    Ok(entries)
}

fn committed_links(
    entries: &[Tracked],
    run: impl Fn(&[&str]) -> Result<Vec<u8>, String>,
) -> Result<BTreeMap<String, String>, String> {
    let mut regular_files = BTreeSet::new();
    let mut links = BTreeMap::new();
    for entry in entries {
        if matches!(entry.kind, TrackedKind::Regular { .. }) {
            regular_files.insert(entry.path.clone());
            continue;
        }
        // OIDs were validated as full hex SHA-1s by parse_listing. Two commands
        // per link with closed stdin, each under COMMAND_TIMEOUT. The size is
        // checked first so a target beyond the store path profile is rejected
        // before its blob is read. Git's fetched object integrity is trusted.
        let size_bytes = run(&["cat-file", "-s", &entry.oid])?;
        let size_text =
            std::str::from_utf8(&size_bytes).map_err(|_| "invalid Git symlink blob size")?;
        let size_text = size_text.strip_suffix('\n').unwrap_or(size_text);
        if size_text.is_empty() || !size_text.bytes().all(|b| b.is_ascii_digit()) {
            return Err("invalid Git symlink blob size".into());
        }
        let size = size_text
            .parse::<usize>()
            .map_err(|_| "Git symlink target exceeds 4096 bytes")?;
        if size > MAX_PATH_BYTES {
            return Err("Git symlink target exceeds 4096 bytes".into());
        }
        let bytes = run(&["cat-file", "blob", &entry.oid])?;
        if bytes.len() != size {
            return Err("Git symlink blob length does not match its declared size".into());
        }
        let target =
            String::from_utf8(bytes).map_err(|_| "non-UTF-8 Git symlink target unsupported")?;
        repository_links::target_path(&entry.path, &target)
            .map_err(|e| format!("invalid committed Git symlink {:?}: {e}", entry.path))?;
        links.insert(entry.path.clone(), target);
    }
    // Include links outside strip_prefix: checkout operates on the full tree.
    repository_links::validate(&regular_files, &links)
        .map_err(|e| format!("invalid complete committed Git symlink namespace: {e}"))?;
    Ok(links)
}

fn safe_relative(text: &str) -> Result<(), String> {
    if text.is_empty()
        || text.len() > MAX_PATH_BYTES
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
            "unsafe Git relative path or strip_prefix {text:?} (maximum 4096 bytes and depth 64)"
        ));
    }
    Ok(())
}

fn export(
    checkout: &Path,
    stage: &Path,
    prefix: &str,
    entries: &[Tracked],
    expected_links: &BTreeMap<String, String>,
) -> Result<(), String> {
    let mut regular_files = BTreeSet::new();
    let mut links = BTreeMap::new();
    for entry in entries {
        check_descendant_directories(
            checkout,
            Path::new(&entry.path).parent().unwrap_or(Path::new("")),
        )?;
        let source = checkout.join(&entry.path);
        match entry.kind {
            TrackedKind::Regular { .. } => {
                regular_metadata(&source)?;
                regular_files.insert(entry.path.clone());
            }
            TrackedKind::Symlink => {
                let metadata = fs::symlink_metadata(&source).map_err(|e| e.to_string())?;
                let target = link_target(&source, &metadata)?;
                if expected_links.get(&entry.path) != Some(&target) {
                    return Err(format!(
                        "Git checkout symlink {:?} does not match committed blob target",
                        entry.path
                    ));
                }
                links.insert(entry.path.clone(), target);
            }
        }
    }
    repository_links::validate(&regular_files, &links)
        .map_err(|e| format!("invalid complete Git symlink namespace: {e}"))?;

    // strip_prefix must be a real directory, not a link, even if its target is
    // otherwise valid. Revalidate literal targets relative to the published root.
    check_descendant_directories(checkout, Path::new(prefix))?;
    let mut selected = Vec::new();
    let mut selected_files = BTreeSet::new();
    let mut selected_links = BTreeMap::new();
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
        match entry.kind {
            TrackedKind::Regular { .. } => {
                selected_files.insert(relative.to_owned());
            }
            TrackedKind::Symlink => {
                selected_links.insert(relative.to_owned(), links[&entry.path].clone());
            }
        }
        selected.push((entry, relative));
    }
    if !prefix.is_empty() && selected.is_empty() {
        return Err("Git strip_prefix has no tracked descendant files".into());
    }
    repository_links::validate(&selected_files, &selected_links)
        .map_err(|e| format!("invalid selected Git symlink namespace after strip_prefix: {e}"))?;

    // Copy regular files first. No link is ever opened, copied or chmodded.
    for (entry, relative) in selected {
        let TrackedKind::Regular { executable } = entry.kind else {
            continue;
        };
        let source = checkout.join(&entry.path);
        regular_metadata(&source)?;
        let target = stage.join(relative);
        ensure_directory(target.parent().ok_or("Git export path has no parent")?)?;
        let mut input = File::open(&source).map_err(|e| e.to_string())?;
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&target)
            .map_err(|e| e.to_string())?;
        std::io::copy(&mut input, &mut output).map_err(|e| e.to_string())?;
        set_mode(&target, if executable { 0o755 } else { 0o644 })?;
    }
    for (relative, raw_target) in selected_links {
        let target = stage.join(relative);
        ensure_directory(target.parent().ok_or("Git symlink path has no parent")?)?;
        create_symlink(&raw_target, &target)?;
    }
    Ok(())
}

fn link_target(path: &Path, metadata: &fs::Metadata) -> Result<String, String> {
    if !metadata.file_type().is_symlink() {
        return Err(format!(
            "Git symlink is not a native symlink: {}",
            path.display()
        ));
    }
    if metadata.len() > MAX_PATH_BYTES as u64 {
        return Err("Git symlink target exceeds 4096 bytes".into());
    }
    let target = fs::read_link(path)
        .map_err(|e| format!("cannot read Git symlink {}: {e}", path.display()))?
        .into_os_string()
        .into_string()
        .map_err(|_| "non-UTF-8 Git symlink target unsupported")?;
    if target.len() > MAX_PATH_BYTES {
        return Err("Git symlink target exceeds 4096 bytes".into());
    }
    Ok(target)
}

fn create_symlink(raw_target: &str, path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(raw_target, path)
            .map_err(|e| format!("cannot create native Git symlink {}: {e}", path.display()))
    }
    #[cfg(not(unix))]
    {
        let _ = (raw_target, path);
        Err("native Git source symlinks require a Unix platform".into())
    }
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

/// Read a whole regular file whose content is needed in memory (the module
/// file, the manifest); no artificial size cap.
fn read_whole(path: &Path) -> Result<Vec<u8>, String> {
    regular_metadata(path)?;
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(|e| e.to_string())?
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
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
    kind: EntryKind,
    mode: u32,
    size: u64,
    sha256: String,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
enum EntryKind {
    Directory,
    File,
    Symlink { target: String },
}

fn snapshot(root: &Path, normalize: bool) -> Result<BTreeMap<String, Entry>, String> {
    fn visit(
        root: &Path,
        path: &Path,
        normalize: bool,
        files: &mut BTreeMap<String, Entry>,
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
        let kind = if metadata.file_type().is_symlink() {
            EntryKind::Symlink {
                target: link_target(path, &metadata)?,
            }
        } else if metadata.is_dir() {
            EntryKind::Directory
        } else {
            regular_metadata(path)?;
            EntryKind::File
        };
        let mut entry = Entry {
            kind,
            mode: mode(&metadata),
            size: 0,
            sha256: String::new(),
        };
        if normalize && !matches!(entry.kind, EntryKind::Symlink { .. }) {
            entry.mode = if entry.kind == EntryKind::Directory || entry.mode & 0o100 != 0 {
                0o755
            } else {
                0o644
            };
            set_mode(path, entry.mode)?;
        }
        if let EntryKind::Symlink { target } = &entry.kind {
            entry.size = target.len() as u64;
        } else if entry.kind == EntryKind::File {
            let mut digest = Sha256::new();
            let mut input = File::open(path).map_err(|e| e.to_string())?;
            entry.size = std::io::copy(&mut input, &mut digest).map_err(|e| e.to_string())?;
            entry.sha256 = format!("{:x}", digest.finalize());
        }
        let directory = entry.kind == EntryKind::Directory;
        files.insert(name, entry);
        if directory {
            for child in fs::read_dir(path).map_err(|e| e.to_string())? {
                visit(
                    root,
                    &child.map_err(|e| e.to_string())?.path(),
                    normalize,
                    files,
                )?;
            }
        }
        Ok(())
    }
    required_directory(root)?;
    let mut files = BTreeMap::new();
    visit(root, root, normalize, &mut files)?;
    let mut regular_files = BTreeSet::new();
    let mut links = BTreeMap::new();
    for (name, entry) in &files {
        match &entry.kind {
            EntryKind::File => {
                regular_files.insert(name.clone());
            }
            EntryKind::Symlink { target } => {
                links.insert(name.clone(), target.clone());
            }
            EntryKind::Directory => {}
        }
    }
    // Actual filesystem namespace, not the manifest or original Git listing:
    // required both after patches and on every warm read, before the callback.
    repository_links::validate(&regular_files, &links)
        .map_err(|e| format!("invalid Git snapshot symlink namespace: {e}"))?;
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

        fn commit_link_blob(&mut self, path: &str, bytes: &[u8]) {
            let blob = self.temp.path().join("link-blob");
            fs::write(&blob, bytes).unwrap();
            let oid = String::from_utf8(self.git(&["hash-object", "-w", blob.to_str().unwrap()]))
                .unwrap();
            self.git(&[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("120000,{},{}", oid.trim(), path),
            ]);
            self.commit_index();
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
    fn rejects_malformed_committed_link_blobs_before_callback_or_publication() {
        for (bytes, message) in [
            (b"data\0../outside".to_vec(), "forbidden character"),
            (b"data\0".to_vec(), "forbidden character"),
            (b"data\xff".to_vec(), "non-UTF-8"),
            (vec![b'a'; 4097], "4096 bytes"),
        ] {
            for prefix in ["", "pkg"] {
                let mut fixture = Fixture::new(prefix);
                // For pkg, this malformed link is outside the selected namespace.
                fs::write(fixture.origin.join("data"), b"original\n").unwrap();
                fixture.git(&["add", "data"]);
                fixture.commit_link_blob("link", &bytes);
                let calls = AtomicUsize::new(0);
                for _ in 0..2 {
                    let result = materialize(
                        &fixture.spec,
                        &LocalPatchPlan::default(),
                        &fixture.cache,
                        true,
                        |_, _| {
                            calls.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        },
                    );
                    assert!(result.is_err(), "malformed blob accepted: {bytes:?}");
                    let error = result.unwrap_err();
                    assert!(error.contains(message), "{error}");
                    assert_eq!(calls.load(Ordering::SeqCst), 0);
                    fixture.assert_no_publication();
                    assert!(
                        fs::read_dir(fixture.cache.join("git/manifests"))
                            .unwrap()
                            .next()
                            .is_none()
                    );
                }
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn preserves_committed_raw_dot_and_parent_targets_cold_and_offline_warm() {
        let mut fixture = Fixture::new("");
        fs::create_dir(fixture.origin.join("dir")).unwrap();
        fs::write(fixture.origin.join("dir/file"), b"tracked directory").unwrap();
        fixture.git(&["add", "dir/file"]);
        let target = "./dir/.././data";
        fixture.commit_link_blob("link", target.as_bytes());
        let tree = fixture.tree();
        let verify = || {
            assert_eq!(
                fs::read_link(tree.root.join("link"))
                    .unwrap()
                    .to_str()
                    .unwrap(),
                target
            );
            assert_eq!(fs::read(tree.root.join("link")).unwrap(), b"original\n");
        };
        verify();
        fs::rename(&fixture.origin, fixture.temp.path().join("offline-origin")).unwrap();
        assert_eq!(fixture.tree(), tree);
        verify();
    }

    #[test]
    #[cfg(unix)]
    fn never_reuses_v2_cache_with_a_truncated_committed_target() {
        let mut fixture = Fixture::new("");
        fixture.commit_link_blob("link", b"data\0../outside");
        let patches = LocalPatchPlan::default();
        let identity = serde_json::to_vec(&(
            "native-git-source-v2",
            (
                &fixture.spec.remote,
                &fixture.spec.commit,
                &fixture.spec.strip_prefix,
            ),
            patches.cache_identity(),
        ))
        .unwrap();
        let key = format!("{:x}", Sha256::digest(identity));
        let root = fixture.cache.join("git/trees").join(&key);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("MODULE.bazel"), MODULE).unwrap();
        fs::write(root.join("data"), b"original\n").unwrap();
        create_symlink("data", &root.join("link")).unwrap();
        let manifest = Manifest {
            policy: "native-git-source-v2".into(),
            key: key.clone(),
            commit: fixture.spec.commit.clone(),
            files: snapshot(&root, true).unwrap(),
        };
        let manifests = fixture.cache.join("git/manifests");
        fs::create_dir_all(&manifests).unwrap();
        fs::write(
            manifests.join(format!("{key}.json")),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let error = materialize(
            &fixture.spec,
            &patches,
            &fixture.cache,
            true,
            |_, _| -> Result<(), String> { panic!("v2 truncated target reached callback") },
        )
        .unwrap_err();
        assert!(error.contains("forbidden character"), "{error}");
        assert!(root.is_dir());
        assert_eq!(fs::read_dir(root.parent().unwrap()).unwrap().count(), 1);
        assert_eq!(fs::read_dir(manifests).unwrap().count(), 1);
        fixture.assert_no_temporaries();
    }

    #[test]
    fn committed_link_reads_reject_bad_sizes_and_inexact_bodies() {
        let entries =
            parse_listing(format!("120000 blob {}\tlink\0", "a".repeat(40)).as_bytes()).unwrap();
        for size in ["4097\n", "999999999999999999999999\n", "-1\n", "4x\n", "\n"] {
            let error = committed_links(&entries, |args| {
                assert_eq!(args[1], "-s", "oversized/invalid blob must not be read");
                Ok(size.as_bytes().to_vec())
            })
            .unwrap_err();
            assert!(
                error.contains("size") || error.contains("4096 bytes"),
                "{error}"
            );
        }
        for body in [b"dat".as_slice(), b"data\0".as_slice()] {
            let error = committed_links(&entries, |args| {
                Ok(if args[1] == "-s" { b"4\n" } else { body }.to_vec())
            })
            .unwrap_err();
            assert!(error.contains("length does not match"), "{error}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn preserves_distroless_relative_link_and_chain_on_cold_and_offline_warm_hits() {
        use std::os::unix::fs::symlink;
        let mut fixture = Fixture::new("");
        fs::write(fixture.origin.join(".bazelversion"), b"8.2.1\n").unwrap();
        fs::create_dir_all(fixture.origin.join("e2e/smoke")).unwrap();
        symlink(
            "../../.bazelversion",
            fixture.origin.join("e2e/smoke/.bazelversion"),
        )
        .unwrap();
        symlink(
            "e2e/smoke/.bazelversion",
            fixture.origin.join("version-chain"),
        )
        .unwrap();
        fixture.commit();
        let tree = fixture.tree();
        let verify = || {
            for (name, target) in [
                ("e2e/smoke/.bazelversion", "../../.bazelversion"),
                ("version-chain", "e2e/smoke/.bazelversion"),
            ] {
                let path = tree.root.join(name);
                let metadata = fs::symlink_metadata(&path).unwrap();
                assert!(metadata.file_type().is_symlink());
                assert_eq!(mode(&metadata), 0o777);
                assert_eq!(fs::read_link(&path).unwrap().to_str().unwrap(), target);
                assert_eq!(fs::read(path).unwrap(), b"8.2.1\n");
            }
        };
        verify();
        fs::rename(&fixture.origin, fixture.temp.path().join("offline-origin")).unwrap();
        assert_eq!(fixture.tree(), tree);
        verify();
        fixture.assert_no_temporaries();
    }

    #[test]
    #[cfg(unix)]
    fn safe_unselected_links_are_validated_but_not_exported() {
        use std::os::unix::fs::symlink;
        let mut fixture = Fixture::new("pkg");
        symlink("pkg/data", fixture.origin.join("outside-link")).unwrap();
        symlink("./data", fixture.origin.join("pkg/inside-link")).unwrap();
        fixture.commit();
        let tree = fixture.tree();
        assert!(fs::symlink_metadata(tree.root.join("outside-link")).is_err());
        assert_eq!(
            fs::read_link(tree.root.join("inside-link"))
                .unwrap()
                .to_str()
                .unwrap(),
            "./data"
        );
        assert_eq!(
            fs::read(tree.root.join("inside-link")).unwrap(),
            b"original\n"
        );
    }

    #[test]
    #[cfg(unix)]
    fn repairs_link_spelling_kind_referent_and_added_links_to_exact_manifest() {
        use std::os::unix::fs::symlink;
        let mut fixture = Fixture::new("");
        symlink("data", fixture.origin.join("link")).unwrap();
        fs::write(fixture.origin.join("same-bytes"), b"original\n").unwrap();
        // Same bytes as the raw link target exercises regular -> link kind changes.
        fs::write(fixture.origin.join("raw-target"), b"data").unwrap();
        fixture.commit();
        let tree = fixture.tree();
        let expected_manifest = fs::read(fixture.manifest(&tree)).unwrap();
        let original = snapshot(&tree.root, false).unwrap();
        assert_eq!(
            original["link"].kind,
            EntryKind::Symlink {
                target: "data".into()
            }
        );
        assert_eq!(original["link"].size, 4);
        assert_eq!(original["link"].sha256, "");
        for corruption in 0..7 {
            let link = tree.root.join("link");
            match corruption {
                0 | 1 => {
                    fs::remove_file(&link).unwrap();
                    symlink(
                        if corruption == 0 {
                            "./data"
                        } else {
                            "same-bytes"
                        },
                        &link,
                    )
                    .unwrap();
                    assert_eq!(fs::read(&link).unwrap(), b"original\n");
                }
                2 => {
                    fs::remove_file(&link).unwrap();
                    fs::write(&link, b"data").unwrap();
                }
                3 => fs::write(tree.root.join("data"), b"corrupted\n").unwrap(),
                4 => symlink("data", tree.root.join("added-link")).unwrap(),
                5 => {
                    fs::remove_file(tree.root.join("raw-target")).unwrap();
                    symlink("data", tree.root.join("raw-target")).unwrap();
                }
                6 => {
                    fs::remove_file(&link).unwrap();
                    symlink("../outside", &link).unwrap();
                }
                _ => unreachable!(),
            }
            let calls = AtomicUsize::new(0);
            let repaired = materialize(
                &fixture.spec,
                &LocalPatchPlan::default(),
                &fixture.cache,
                true,
                |root, _| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(snapshot(root, false).unwrap(), original);
                    Ok(())
                },
            )
            .unwrap()
            .0;
            assert_eq!(repaired, tree);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                fs::read(fixture.manifest(&tree)).unwrap(),
                expected_manifest
            );
            assert!(
                fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(fs::read_link(&link).unwrap(), Path::new("data"));
            assert_eq!(fs::read(&link).unwrap(), b"original\n");
            fixture.assert_no_temporaries();
        }
    }

    #[test]
    #[cfg(unix)]
    fn rejects_unsafe_targets_without_callback_or_publication() {
        use std::os::unix::fs::symlink;
        for (target, message) in [
            ("/etc/passwd", "absolute"),
            ("../outside", "escapes"),
            ("absent", "dangling"),
            ("link", "cycle"),
            ("dir", "directory target"),
            ("missing/../data", "missing intermediate"),
            ("data/../data", "intermediate file"),
            (".git/config", "missing intermediate"),
        ] {
            let mut fixture = Fixture::new("");
            fs::create_dir(fixture.origin.join("dir")).unwrap();
            fs::write(fixture.origin.join("dir/file"), b"data").unwrap();
            symlink(target, fixture.origin.join("link")).unwrap();
            fixture.commit();
            let error = materialize(
                &fixture.spec,
                &LocalPatchPlan::default(),
                &fixture.cache,
                true,
                |_, _| -> Result<(), String> { panic!("unsafe link reached callback") },
            )
            .unwrap_err();
            assert!(error.contains(message), "{target}: {error}");
            fixture.assert_no_publication();
        }
    }

    #[cfg(unix)]
    include!("../tests/support/forty_link_read.rs");

    #[test]
    #[cfg(unix)]
    fn rejects_multiple_link_cycle_and_excessive_hops_but_preserves_forty_hops() {
        use std::os::unix::fs::symlink;
        for cycle in [false, true] {
            let mut fixture = Fixture::new("");
            for index in 0..40 {
                let target = if index == 39 {
                    "data".into()
                } else {
                    format!("link{}", index + 1)
                };
                symlink(target, fixture.origin.join(format!("link{index}"))).unwrap();
            }
            fixture.commit();
            let tree = fixture.tree();
            assert_eq!(
                read_through_forty_links(&tree.root.join("link0")),
                b"original\n"
            );
            let rejected_cache = fixture.temp.path().join("rejected-cache");
            if cycle {
                fs::remove_file(fixture.origin.join("link39")).unwrap();
                symlink("link0", fixture.origin.join("link39")).unwrap();
            } else {
                symlink("link0", fixture.origin.join("link40")).unwrap();
            }
            fixture.commit();
            fixture.cache = rejected_cache;
            let error = materialize(
                &fixture.spec,
                &LocalPatchPlan::default(),
                &fixture.cache,
                true,
                |_, _| Ok(()),
            )
            .unwrap_err();
            assert!(
                error.contains(if cycle { "cycle" } else { "40 steps" }),
                "{error}"
            );
            fixture.assert_no_publication();
        }
    }

    #[test]
    #[cfg(unix)]
    fn selected_namespace_rejects_links_that_escape_the_stripped_root() {
        use std::os::unix::fs::symlink;
        let mut fixture = Fixture::new("pkg");
        symlink("../outside", fixture.origin.join("pkg/link")).unwrap();
        fixture.commit();
        let error = materialize(
            &fixture.spec,
            &LocalPatchPlan::default(),
            &fixture.cache,
            true,
            |_, _| Ok(()),
        )
        .unwrap_err();
        assert!(error.contains("after strip_prefix"), "{error}");
        assert!(error.contains("escapes"), "{error}");
        fixture.assert_no_publication();
    }

    #[test]
    #[cfg(unix)]
    fn symlink_strip_prefix_and_linked_module_remain_explicitly_unsupported() {
        use std::os::unix::fs::symlink;
        let mut fixture = Fixture::new("");
        symlink("data", fixture.origin.join("prefix-link")).unwrap();
        fixture.commit();
        fixture.spec.strip_prefix = "prefix-link".into();
        let error = materialize(
            &fixture.spec,
            &LocalPatchPlan::default(),
            &fixture.cache,
            true,
            |_, _| Ok(()),
        )
        .unwrap_err();
        assert!(error.contains("not a regular directory"), "{error}");
        fixture.assert_no_publication();
        fixture.spec.strip_prefix.clear();
        fs::rename(
            fixture.origin.join("MODULE.bazel"),
            fixture.origin.join("module-source"),
        )
        .unwrap();
        symlink("module-source", fixture.origin.join("MODULE.bazel")).unwrap();
        fixture.commit();
        let error = materialize(
            &fixture.spec,
            &LocalPatchPlan::default(),
            &fixture.cache,
            true,
            |_, _| -> Result<(), String> { panic!("linked module reached callback") },
        )
        .unwrap_err();
        assert!(
            error.contains("not regular") && error.contains("MODULE.bazel"),
            "{error}"
        );
        fixture.assert_no_publication();
    }

    #[test]
    #[cfg(unix)]
    fn patching_referent_preserves_link_and_warm_updated_contents() {
        use std::os::unix::fs::symlink;
        let mut fixture = Fixture::new("");
        symlink("./data", fixture.origin.join("link")).unwrap();
        fixture.commit();
        let plan = patch_plan(fixture.temp.path(), &change("data", "original", "patched"));
        let validate = |root: &Path, _: &[u8]| {
            assert_eq!(fs::read(root.join("link")).unwrap(), b"patched\n");
            assert_eq!(
                fs::read_link(root.join("link")).unwrap().to_str().unwrap(),
                "./data"
            );
            Ok(())
        };
        let tree = materialize(&fixture.spec, &plan, &fixture.cache, true, validate)
            .unwrap()
            .0;
        fs::rename(&fixture.origin, fixture.temp.path().join("offline-origin")).unwrap();
        assert_eq!(
            materialize(&fixture.spec, &plan, &fixture.cache, true, validate)
                .unwrap()
                .0,
            tree
        );
    }

    #[test]
    #[cfg(unix)]
    fn patch_deleting_referent_is_rejected_by_post_patch_snapshot() {
        use std::os::unix::fs::symlink;
        let mut fixture = Fixture::new("");
        symlink("data", fixture.origin.join("link")).unwrap();
        fixture.commit();
        let plan = patch_plan(
            fixture.temp.path(),
            "diff --git a/data b/data\n--- a/data\n+++ /dev/null\n@@ -1 +0,0 @@\n-original\n",
        );
        let error = materialize(
            &fixture.spec,
            &plan,
            &fixture.cache,
            true,
            |_, _| -> Result<(), String> { panic!("dangling post-patch link reached callback") },
        )
        .unwrap_err();
        assert!(
            error.contains("snapshot") && error.contains("dangling"),
            "{error}"
        );
        fixture.assert_no_publication();
    }

    #[test]
    #[cfg(unix)]
    fn patches_replace_link_leaves_and_read_unused_new_link_paths_like_patch_util() {
        use std::os::unix::fs::symlink;
        for (text, link_patched) in [
            (change("link", "original", "patched"), true),
            (
                "diff --git a/data b/link\n--- a/data\n+++ b/link\n@@ -1 +1 @@\n-original\n+patched\n"
                    .into(),
                false,
            ),
        ] {
            let mut fixture = Fixture::new("");
            symlink("data", fixture.origin.join("link")).unwrap();
            fixture.commit();
            let plan = patch_plan(fixture.temp.path(), &text);
            let (git_tree, _) = materialize(&fixture.spec, &plan, &fixture.cache, true, |_, _| Ok(()))
                .unwrap();
            let link = fs::symlink_metadata(git_tree.root.join("link")).unwrap();
            if link_patched {
                // PatchUtil deletes the link and writes a fresh regular file;
                // the referent is untouched.
                assert!(link.is_file());
                assert_eq!(fs::read(git_tree.root.join("link")).unwrap(), b"patched\n");
                assert_eq!(fs::read(git_tree.root.join("data")).unwrap(), b"original\n");
            } else {
                // The existing old path is patched; the link still points at it.
                assert!(link.file_type().is_symlink());
                assert_eq!(fs::read(git_tree.root.join("data")).unwrap(), b"patched\n");
            }
            assert_eq!(
                fs::read(fixture.origin.join("data")).unwrap(),
                b"original\n"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn post_patch_empty_intermediate_directory_fails_closed_with_implicit_directory_helper() {
        use std::os::unix::fs::symlink;
        let mut fixture = Fixture::new("");
        fs::create_dir(fixture.origin.join("dir")).unwrap();
        fs::write(fixture.origin.join("dir/only-file"), b"delete\n").unwrap();
        symlink("dir/../data", fixture.origin.join("link")).unwrap();
        fixture.commit();
        let plan = patch_plan(
            fixture.temp.path(),
            "diff --git a/dir/only-file b/dir/only-file\n--- a/dir/only-file\n+++ /dev/null\n@@ -1 +0,0 @@\n-delete\n",
        );
        let error =
            materialize(&fixture.spec, &plan, &fixture.cache, true, |_, _| Ok(())).unwrap_err();
        assert!(
            error.contains("snapshot") && error.contains("missing intermediate directory"),
            "{error}"
        );
        fixture.assert_no_publication();
    }

    #[test]
    #[cfg(unix)]
    fn export_rejects_changed_or_missing_expected_literal_targets_before_copying() {
        let temp = tempfile::tempdir().unwrap();
        let checkout = temp.path().join("checkout");
        let stage = temp.path().join("stage");
        fs::create_dir(&checkout).unwrap();
        fs::create_dir(&stage).unwrap();
        fs::write(checkout.join("data"), b"original").unwrap();
        create_symlink("./data", &checkout.join("link")).unwrap();
        let oid = "a".repeat(40);
        let entries = parse_listing(
            format!("100644 blob {oid}\tdata\x00120000 blob {oid}\tlink\0").as_bytes(),
        )
        .unwrap();
        for expected in [
            BTreeMap::new(),
            BTreeMap::from([("link".into(), "data".into())]),
        ] {
            let error = export(&checkout, &stage, "", &entries, &expected).unwrap_err();
            assert!(error.contains("does not match committed blob"), "{error}");
            assert!(fs::read_dir(&stage).unwrap().next().is_none());
        }
    }

    #[test]
    #[cfg(unix)]
    fn export_checks_expected_native_kind_and_never_follows_link_parents() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let checkout = temp.path().join("checkout");
        let stage = temp.path().join("stage");
        fs::create_dir(&checkout).unwrap();
        fs::create_dir(&stage).unwrap();
        fs::write(checkout.join("data"), b"original").unwrap();
        let entries = [Tracked {
            path: "data".into(),
            kind: TrackedKind::Symlink,
            oid: "a".repeat(40),
        }];
        assert!(
            export(&checkout, &stage, "", &entries, &BTreeMap::new())
                .unwrap_err()
                .contains("not a native symlink")
        );
        symlink("data", checkout.join("link")).unwrap();
        let entries = [Tracked {
            path: "link".into(),
            kind: TrackedKind::Regular { executable: false },
            oid: "a".repeat(40),
        }];
        assert!(
            export(&checkout, &stage, "", &entries, &BTreeMap::new())
                .unwrap_err()
                .contains("not regular")
        );
        let entries = [Tracked {
            path: "link/child".into(),
            kind: TrackedKind::Regular { executable: false },
            oid: "a".repeat(40),
        }];
        assert!(
            export(&checkout, &stage, "", &entries, &BTreeMap::new())
                .unwrap_err()
                .contains("not a regular directory")
        );
        assert!(fs::read_dir(&stage).unwrap().next().is_none());
        assert_eq!(fs::read(checkout.join("data")).unwrap(), b"original");
    }

    #[test]
    #[cfg(unix)]
    fn snapshot_rejects_non_utf8_symlink_targets_without_following_them() {
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        symlink(
            std::ffi::OsString::from_vec(vec![255]),
            temp.path().join("link"),
        )
        .unwrap();
        assert!(
            snapshot(temp.path(), true)
                .unwrap_err()
                .contains("non-UTF-8 Git symlink target")
        );
    }

    #[test]
    #[cfg(unix)]
    fn rejects_unsafe_tracked_symlink_even_outside_strip_prefix_before_publication() {
        use std::os::unix::fs::symlink;
        let mut fixture = Fixture::new("pkg");
        symlink("../outside", fixture.origin.join("link")).unwrap();
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
            .contains("escapes")
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
            ("100600", "blob", "special"),
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
        // No listing-size or entry-count cap (former 1 MiB / 100000 limits).
        let many: String = (0..100_001)
            .map(|i| format!("100644 blob {sha}\tf{i}\0"))
            .collect();
        assert!(many.len() > 1024 * 1024);
        assert_eq!(parse_listing(many.as_bytes()).unwrap().len(), 100_001);
        assert!(safe_relative(&vec!["x"; 65].join("/")).is_err());
        assert!(verify_commit(format!("{}\n", "b".repeat(40)).as_bytes(), &sha).is_err());
        assert_eq!(COMMAND_TIMEOUT, Duration::from_secs(600));
    }

    #[test]
    fn listing_parser_recognizes_regular_executable_and_symlink_kinds() {
        let sha = "a".repeat(40);
        for (mode, expected) in [
            ("100644", TrackedKind::Regular { executable: false }),
            ("100755", TrackedKind::Regular { executable: true }),
            ("120000", TrackedKind::Symlink),
        ] {
            let entries = parse_listing(format!("{mode} blob {sha}\tpath\0").as_bytes()).unwrap();
            assert_eq!(entries[0].kind, expected);
            assert_eq!(entries[0].path, "path");
            assert_eq!(entries[0].oid, sha);
        }
        assert!(parse_listing(b"120000 blob bad-oid\tlink\0").is_err());
        assert!(safe_relative(&"x".repeat(MAX_PATH_BYTES + 1)).is_err());
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
    fn snapshot_rejects_hardlinks_dangling_symlinks_non_utf8_metadata_and_size_overflows() {
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(root.join("file"), "data").unwrap();
        fs::hard_link(root.join("file"), root.join("hard")).unwrap();
        assert!(snapshot(root, false).unwrap_err().contains("hardlinked"));
        fs::remove_file(root.join("hard")).unwrap();
        symlink("missing", root.join("link")).unwrap();
        assert!(snapshot(root, false).is_err());
        fs::remove_file(root.join("link")).unwrap();
        let non_utf8 = root.join(std::ffi::OsString::from_vec(vec![255]));
        fs::write(&non_utf8, "data").unwrap();
        assert!(snapshot(root, false).unwrap_err().contains("non-UTF-8"));
        fs::remove_file(non_utf8).unwrap();
        fs::create_dir(root.join(".git")).unwrap();
        assert!(snapshot(root, false).is_err());
        fs::remove_dir(root.join(".git")).unwrap();
        // No size caps (the former 1 GiB tree / 16 MiB module limits).
        File::create(root.join("huge"))
            .unwrap()
            .set_len(17 * 1024 * 1024)
            .unwrap();
        assert_eq!(
            snapshot(root, false).unwrap()["huge"].size,
            17 * 1024 * 1024
        );
        fs::remove_file(root.join("huge")).unwrap();
        File::create(root.join("MODULE.bazel"))
            .unwrap()
            .set_len(17 * 1024 * 1024)
            .unwrap();
        assert_eq!(
            read_whole(&root.join("MODULE.bazel")).unwrap().len(),
            17 * 1024 * 1024
        );
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
