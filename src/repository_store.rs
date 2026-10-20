//! Verified content-addressed output for the direct file/template subset.
//!
//! Evaluation is never cached here: the callback runs before acquiring a
//! publication lock, including on warm calls. The cache is user-owned, not a
//! sandbox or a remote-cache protocol. Callbacks must finish all their writes
//! before returning; callers must treat returned trees as immutable.
//!
//! Files are hashed incrementally while snapshotting (constant memory). There
//! are no file-size, total-size, entry-count or manifest-size caps; only the
//! store's path profile (UTF-8, 4096-byte names, depth 64) is enforced.

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

const PROFILE: &str = "direct-file-template-v1";
const DEPTH_LIMIT: usize = 64;
const PATH_LIMIT: usize = 4096;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: u32,
    profile: String,
    identity: String,
    files: BTreeMap<String, Entry>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    directory: bool,
    mode: u32,
    size: u64,
    sha256: String,
    /// Symbolic link target bytes (UTF-8), never followed. Created only by
    /// `repository_ctx.symlink` / executed tools (StarlarkRepositoryContext.java:
    /// 196-256). Omitted for files and directories, keeping legacy manifests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    symlink: Option<String>,
}

/// Execute into a private stage, then verify and publish its complete tree.
/// `execute` returns exact serialized runtime input provenance, after validating
/// its result. `provenance` identifies the definition, attributes and mappings.
/// Neither byte string may contain the incidental stage pathname; the stage
/// pathname itself is derived from `stage_key` (the repository's canonical
/// name) so repository content that embeds its own working directory —
/// `ctx.path(".")`, as gazelle's `go_env` does — addresses a path that is
/// identical in every process and invocation, keeping the tree identity
/// stable. Concurrent evaluations of the same key serialize on a stage lock
/// in the cache, like module extension working directories.
pub fn materialize<F>(
    cache: &Path,
    stage_key: &str,
    provenance: &[u8],
    execute: F,
) -> Result<PathBuf, String>
where
    F: FnOnce(&Path) -> Result<Vec<u8>, String>,
{
    ensure_directory(cache)?;
    let direct = cache.join("direct");
    let trees = direct.join("trees");
    let manifests = direct.join("manifests");
    let locks = direct.join("locks");
    let stages = direct.join("stages");
    for path in [&trees, &manifests, &locks, &stages] {
        ensure_directory(path)?;
    }
    // 128 bits: two repositories sharing a slug would share a stage.
    let slug = blake3::hash(stage_key.as_bytes()).to_hex()[..32].to_owned();
    let stage_lock_path = locks.join(format!("stage-{slug}"));
    if let Some(metadata) = optional_metadata(&stage_lock_path)? {
        regular_metadata(&metadata)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    no_follow(&mut options);
    let stage_lock = options.open(&stage_lock_path).map_err(|e| e.to_string())?;
    regular_metadata(&stage_lock.metadata().map_err(|e| e.to_string())?)?;
    FileExt::lock_exclusive(&stage_lock)
        .map_err(|e| format!("cannot lock direct repository stage: {e}"))?;
    // Held until return: the stage pathname exists only under this lock.
    let _stage_guard = stage_lock;
    let stage_path = stages.join(&slug);
    if optional_metadata(&stage_path)?.is_some() {
        crate::repository_ctx::remove_tree(&stage_path)
            .map_err(|e| format!("cannot remove interrupted direct repository stage: {e}"))?;
    }
    let stage = tempfile::Builder::new()
        .prefix(&slug)
        .rand_bytes(0)
        .tempdir_in(&stages)
        .map_err(|e| format!("cannot stage direct repository: {e}"))?;
    // TempDir owns cleanup on callback errors and Rust unwinding as well.
    let inputs = execute(stage.path())?;
    required_directory(stage.path())?;
    ensure_boundary(stage.path())?;
    let files = snapshot(stage.path())?;
    let encoded_snapshot = serde_json::to_vec(&files).map_err(|e| e.to_string())?;
    let mut digest = Sha256::new();
    digest.update(PROFILE.as_bytes());
    digest.update([0]);
    for bytes in [provenance, inputs.as_slice(), encoded_snapshot.as_slice()] {
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    let identity = format!("{:x}", digest.finalize());
    let manifest = Manifest {
        format: 1,
        profile: PROFILE.into(),
        identity: identity.clone(),
        files,
    };
    let encoded = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;

    // Recheck parents after the callback, before touching persistent entries.
    for path in [&trees, &manifests, &locks, &stages] {
        ensure_directory(path)?;
    }
    let lock_path = locks.join(&identity);
    if let Some(metadata) = optional_metadata(&lock_path)? {
        regular_metadata(&metadata)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    no_follow(&mut options);
    let lock = options.open(&lock_path).map_err(|e| e.to_string())?;
    regular_metadata(&lock.metadata().map_err(|e| e.to_string())?)?;
    FileExt::lock_exclusive(&lock).map_err(|e| format!("cannot lock direct repository: {e}"))?;
    // The File releases the lock on every exit. Dependencies have already run.
    let destination = trees.join(&identity);
    let manifest_path = manifests.join(format!("{identity}.json"));
    if read_stable(&manifest_path)
        .and_then(|bytes| serde_json::from_slice::<Manifest>(&bytes).map_err(|e| e.to_string()))
        .is_ok_and(|existing| existing == manifest)
        && snapshot(&destination).is_ok_and(|actual| actual == manifest.files)
    {
        return Ok(destination);
    }

    // Prepare metadata before replacing anything. Quarantine names are private
    // to this publisher, and renaming a corrupt symlink never follows it.
    let mut metadata_stage = tempfile::NamedTempFile::new_in(&manifests)
        .map_err(|e| format!("cannot stage direct manifest: {e}"))?;
    metadata_stage
        .write_all(&encoded)
        .and_then(|()| metadata_stage.as_file().sync_all())
        .map_err(|e| e.to_string())?;
    let trash = tempfile::Builder::new()
        .prefix(".trash-")
        .tempdir_in(&trees)
        .map_err(|e| e.to_string())?;
    quarantine(&destination, &trash.path().join("tree"))?;
    quarantine(&manifest_path, &trash.path().join("manifest"))?;
    fs::rename(stage.path(), &destination)
        .map_err(|e| format!("cannot publish direct repository: {e}"))?;
    // These are two atomic renames, not an atomic pair. A failure/crash leaves
    // missing metadata, which forces repair on the next evaluated invocation.
    metadata_stage
        .persist(&manifest_path)
        .map_err(|e| format!("cannot publish direct manifest: {e}"))?;
    for path in [&trees, &manifests] {
        File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|e| e.to_string())?;
    }
    // TempDir removal does not follow symlinks inside our quarantine.
    trash
        .close()
        .map_err(|e| format!("cannot remove direct quarantine: {e}"))?;
    Ok(destination)
}

fn optional_metadata(path: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot inspect {}: {error}", path.display())),
    }
}

fn required_directory(path: &Path) -> Result<(), String> {
    if !fs::symlink_metadata(path)
        .map_err(|e| e.to_string())?
        .is_dir()
    {
        return Err(format!(
            "direct repository requires a real directory: {}",
            path.display()
        ));
    }
    Ok(())
}

fn ensure_directory(path: &Path) -> Result<(), String> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("direct repository cache must be absolute without '..'".into());
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        match fs::create_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("cannot create {}: {error}", current.display())),
        }
        required_directory(&current)?;
    }
    Ok(())
}

fn ensure_boundary(root: &Path) -> Result<(), String> {
    // Bazel 9.3 RepositoryFetchFunction / RepositoryUtils.isValidRepoRoot.
    for name in ["REPO.bazel", "MODULE.bazel", "WORKSPACE.bazel", "WORKSPACE"] {
        if optional_metadata(&root.join(name))?.is_some_and(|metadata| metadata.is_file()) {
            return Ok(());
        }
    }
    let path = root.join("REPO.bazel");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o644);
    }
    let file = options
        .open(path)
        .map_err(|e| format!("cannot create REPO.bazel: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o644))
            .map_err(|e| e.to_string())?;
    }
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

fn quarantine(path: &Path, target: &Path) -> Result<(), String> {
    if optional_metadata(path)?.is_some() {
        fs::rename(path, target)
            .map_err(|e| format!("cannot quarantine {}: {e}", path.display()))?;
    }
    Ok(())
}

fn regular_metadata(metadata: &fs::Metadata) -> Result<(), String> {
    if !metadata.is_file() {
        return Err(
            "direct repository accepts only regular files and directories (no symlinks)".into(),
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err("hard-linked direct repository files are unsupported".into());
        }
    }
    Ok(())
}

fn no_follow(options: &mut OpenOptions) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Linux O_NOFOLLOW | O_NONBLOCK: reject links and never block on a FIFO
        // substituted between lstat and open. No unsafe code or new dependency.
        options.custom_flags(0x20000 | 0x800);
    }
    #[cfg(not(target_os = "linux"))]
    let _ = options;
}

fn stable(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    if before.file_type() != after.file_type()
        || before.len() != after.len()
        || before.permissions() != after.permissions()
        || before.modified().ok() != after.modified().ok()
    {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev()
            || before.ino() != after.ino()
            || before.nlink() != after.nlink()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
        {
            return false;
        }
    }
    true
}

/// Read a whole regular file that must not change while it is read.
fn read_stable(path: &Path) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    stream_stable(path, &mut bytes)?;
    Ok(bytes)
}

/// SHA-256 and size of a regular file, hashed incrementally (constant memory).
fn hash_stable(path: &Path) -> Result<(u64, String), String> {
    let mut digest = Sha256::new();
    let size = stream_stable(path, &mut digest)?;
    Ok((size, format!("{:x}", digest.finalize())))
}

/// Stream a regular file into `sink`, rejecting symlinks and any change of
/// identity or size while it is read.
fn stream_stable(path: &Path, sink: &mut dyn Write) -> Result<u64, String> {
    let before = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    regular_metadata(&before)?;
    let mut options = OpenOptions::new();
    options.read(true);
    no_follow(&mut options);
    let mut file = options.open(path).map_err(|e| e.to_string())?;
    let opened = file.metadata().map_err(|e| e.to_string())?;
    regular_metadata(&opened)?;
    if !stable(&before, &opened) {
        return Err("direct repository file changed while opening".into());
    }
    let copied = std::io::copy(&mut Read::by_ref(&mut file), sink).map_err(|e| e.to_string())?;
    if copied != before.len()
        || !stable(&before, &file.metadata().map_err(|e| e.to_string())?)
        || !stable(
            &before,
            &fs::symlink_metadata(path).map_err(|e| e.to_string())?,
        )
    {
        return Err("direct repository file changed while reading".into());
    }
    Ok(copied)
}

fn mode(metadata: &fs::Metadata) -> Result<u32, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode();
        if mode & 0o7000 != 0 {
            return Err("special permission bits in direct repository are unsupported".into());
        }
        Ok(mode & 0o777)
    }
    #[cfg(not(unix))]
    {
        Ok(u32::from(metadata.permissions().readonly()))
    }
}

fn safe_relative(name: &str) -> Result<(), String> {
    if name.len() > PATH_LIMIT
        || name.split('/').count() > DEPTH_LIMIT
        || name.contains(['\\', '\0', ':', '?', '#', '%'])
        || name.chars().any(char::is_control)
        || name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || Path::new(name)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("invalid direct repository path or path/depth limit exceeded".into());
    }
    Ok(())
}

fn snapshot(root: &Path) -> Result<BTreeMap<String, Entry>, String> {
    fn visit(root: &Path, path: &Path, files: &mut BTreeMap<String, Entry>) -> Result<(), String> {
        let before = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
        let name = path
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .to_str()
            .ok_or("non-UTF-8 direct repository path")?
            .to_owned();
        if !name.is_empty() {
            safe_relative(&name)?;
        }
        if before.file_type().is_symlink() {
            if name.is_empty() {
                return Err("direct repository root must not be a symlink".into());
            }
            let target = fs::read_link(path).map_err(|e| e.to_string())?;
            let target = target
                .to_str()
                .filter(|target| {
                    !target.is_empty() && target.len() <= PATH_LIMIT && !target.contains('\0')
                })
                .ok_or(
                    "direct repository symlink target must be nonempty UTF-8 within the path limit",
                )?
                .to_owned();
            if !stable(
                &before,
                &fs::symlink_metadata(path).map_err(|e| e.to_string())?,
            ) {
                return Err("direct repository changed during snapshot".into());
            }
            files.insert(
                name,
                Entry {
                    directory: false,
                    mode: 0,
                    size: target.len() as u64,
                    sha256: format!("{:x}", Sha256::digest(target.as_bytes())),
                    symlink: Some(target),
                },
            );
            return Ok(());
        }
        let mut entry = Entry {
            directory: before.is_dir(),
            mode: mode(&before)?,
            size: 0,
            sha256: String::new(),
            symlink: None,
        };
        if !entry.directory {
            regular_metadata(&before)?;
            let (size, sha256) = hash_stable(path)?;
            entry.size = size;
            entry.sha256 = sha256;
        }
        let directory = entry.directory;
        files.insert(name, entry);
        if directory {
            for child in fs::read_dir(path).map_err(|e| e.to_string())? {
                visit(root, &child.map_err(|e| e.to_string())?.path(), files)?;
            }
        }
        if !stable(
            &before,
            &fs::symlink_metadata(path).map_err(|e| e.to_string())?,
        ) {
            return Err("direct repository changed during snapshot".into());
        }
        Ok(())
    }
    required_directory(root)?;
    let mut files = BTreeMap::new();
    visit(root, root, &mut files)?;
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};

    fn temp() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("repository-store-test-")
            .tempdir_in("/tmp/opencode")
            .unwrap()
    }

    fn write(stage: &Path) -> Result<Vec<u8>, String> {
        fs::write(stage.join("data"), [0, 255, 128, 10]).map_err(|e| e.to_string())?;
        fs::create_dir(stage.join("empty")).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(stage.join("data"), fs::Permissions::from_mode(0o755)).unwrap();
        }
        Ok(b"exact runtime inputs".to_vec())
    }

    fn metadata_path(cache: &Path, tree: &Path) -> PathBuf {
        cache.join("direct/manifests").join(format!(
            "{}.json",
            tree.file_name().unwrap().to_str().unwrap()
        ))
    }

    fn no_stages(cache: &Path) {
        assert!(
            fs::read_dir(cache.join("direct/trees"))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_str()
                    .unwrap()
                    .starts_with('.'))
        );
        // Deterministic stages live in their own directory and never survive
        // a completed materialization.
        assert_eq!(
            fs::read_dir(cache.join("direct/stages"))
                .map(|entries| entries.count())
                .unwrap_or(0),
            0
        );
    }

    #[test]
    fn stage_paths_are_deterministic_across_materializations() {
        // Repository content may embed its own working directory (gazelle's
        // go_env writes `ctx.path(".")` into go.env). With a stage keyed by
        // the repository name, that path is a pure function of the cache and
        // the key, so re-evaluation reproduces the identity and warm reuse
        // holds — in this process and in any other sharing the cache.
        let cache = temp();
        let embed = |stage: &Path| {
            fs::write(
                stage.join("go.env"),
                format!("GOPATH='{}'\n", stage.display()),
            )
            .map_err(|e| e.to_string())?;
            Ok(b"inputs".to_vec())
        };
        let first =
            materialize(cache.path(), "rules_gazelle++go_env", b"provenance", embed).unwrap();
        // The embedded working directory is the deterministic stage:
        // <cache>/direct/stages/<32-hex slug of the key>.
        let embedded = fs::read_to_string(first.join("go.env")).unwrap();
        let stages_dir = cache.path().join("direct/stages");
        assert!(embedded.starts_with(&format!("GOPATH='{}", stages_dir.display())));
        assert_eq!(
            embedded
                .lines()
                .next()
                .unwrap()
                .trim_end_matches('\'')
                .rsplit('/')
                .next()
                .unwrap()
                .len(),
            32
        );
        let second =
            materialize(cache.path(), "rules_gazelle++go_env", b"provenance", embed).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            fs::read_dir(cache.path().join("direct/trees"))
                .unwrap()
                .count(),
            1
        );
        // A different repository embeds a different stage pathname and gets
        // a different content address.
        assert_ne!(
            materialize(cache.path(), "other_repo", b"provenance", embed).unwrap(),
            first
        );
        no_stages(cache.path());
    }

    #[test]
    fn interrupted_stage_directory_is_replaced_under_its_lock() {
        let cache = temp();
        let slug = blake3::hash(b"stale-key").to_hex()[..32].to_owned();
        let stale = cache.path().join("direct/stages").join(&slug);
        fs::create_dir_all(&stale).unwrap();
        fs::write(stale.join("leftover"), b"interrupted").unwrap();
        let tree = materialize(cache.path(), "stale-key", b"", write).unwrap();
        assert_eq!(fs::read(tree.join("data")).unwrap(), [0, 255, 128, 10]);
        assert!(!tree.join("leftover").exists());
        no_stages(cache.path());
    }

    #[test]
    fn binary_executable_empty_directories_and_boundary() {
        let cache = temp();
        let tree = materialize(cache.path(), "test-key", b"definition", write).unwrap();
        assert_eq!(fs::read(tree.join("data")).unwrap(), [0, 255, 128, 10]);
        assert!(tree.join("empty").is_dir());
        assert_eq!(fs::read(tree.join("REPO.bazel")).unwrap(), b"");
        assert!(!tree.join("MODULE.bazel").exists());
        assert!(!tree.join("BUILD").exists());
        #[cfg(unix)]
        assert_eq!(
            mode(&fs::metadata(tree.join("data")).unwrap()).unwrap(),
            0o755
        );
        let manifest: Manifest =
            serde_json::from_slice(&fs::read(metadata_path(cache.path(), &tree)).unwrap()).unwrap();
        assert!(manifest.files["empty"].directory);
        assert!(manifest.files[""].directory);
        assert_eq!(manifest.files, snapshot(&tree).unwrap());
        no_stages(cache.path());
    }

    #[test]
    fn provenance_and_runtime_inputs_are_framed_and_part_of_identity() {
        let cache = temp();
        let a = materialize(cache.path(), "test-key", b"a", |_| Ok(b"bc".to_vec())).unwrap();
        let b = materialize(cache.path(), "test-key", b"ab", |_| Ok(b"c".to_vec())).unwrap();
        let c = materialize(cache.path(), "test-key", b"a", |_| Ok(b"bd".to_vec())).unwrap();
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn warm_calls_always_execute_and_capture_current_inputs() {
        let cache = temp();
        let count = AtomicUsize::new(0);
        let run = |input: &[u8]| {
            materialize(cache.path(), "test-key", b"same", |stage| {
                count.fetch_add(1, Ordering::SeqCst);
                write(stage)?;
                Ok(input.to_vec())
            })
            .unwrap()
        };
        let a = run(b"first");
        assert_eq!(a, run(b"first"));
        assert_ne!(a, run(b"changed"));
        assert_eq!(count.load(Ordering::SeqCst), 3);
        no_stages(cache.path());
    }

    #[test]
    fn callback_error_and_panic_clean_private_stage_without_publication() {
        let cache = temp();
        assert!(
            materialize(cache.path(), "test-key", b"", |stage| {
                write(stage)?;
                Err("callback failure".into())
            })
            .is_err()
        );
        let result = std::panic::catch_unwind(|| {
            materialize(cache.path(), "test-key", b"", |stage| {
                write(stage).unwrap();
                panic!("callback panic");
            })
        });
        assert!(result.is_err());
        for name in ["trees", "manifests"] {
            assert_eq!(
                fs::read_dir(cache.path().join("direct").join(name))
                    .unwrap()
                    .count(),
                0
            );
        }
        // The stage lock file persists like a publication lock (deleting a
        // lock file that other processes may hold would break exclusion);
        // no stage directory and no tree survive the failure.
        assert!(
            fs::read_dir(cache.path().join("direct/locks"))
                .unwrap()
                .all(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_str()
                    .unwrap()
                    .starts_with("stage-"))
        );
        no_stages(cache.path());
    }

    #[test]
    fn recognized_boundaries_are_preserved_build_is_not_a_boundary() {
        let cache = temp();
        for boundary in [
            "REPO.bazel",
            "MODULE.bazel",
            "WORKSPACE.bazel",
            "WORKSPACE",
            "BUILD",
            "BUILD.bazel",
        ] {
            let tree = materialize(cache.path(), "test-key", boundary.as_bytes(), |stage| {
                fs::write(stage.join(boundary), b"original").unwrap();
                Ok(vec![])
            })
            .unwrap();
            assert_eq!(fs::read(tree.join(boundary)).unwrap(), b"original");
            assert_eq!(
                tree.join("REPO.bazel").exists(),
                boundary == "REPO.bazel" || boundary.starts_with("BUILD")
            );
            assert_eq!(
                tree.join("MODULE.bazel").exists(),
                boundary == "MODULE.bazel"
            );
        }
    }

    #[test]
    fn corruption_of_bytes_mode_and_empty_directory_is_repaired() {
        let cache = temp();
        let tree = materialize(cache.path(), "test-key", b"", write).unwrap();
        let original = snapshot(&tree).unwrap();
        fs::write(tree.join("data"), b"bad").unwrap();
        assert_eq!(
            tree,
            materialize(cache.path(), "test-key", b"", write).unwrap()
        );
        assert_eq!(original, snapshot(&tree).unwrap());
        fs::remove_dir(tree.join("empty")).unwrap();
        assert_eq!(
            tree,
            materialize(cache.path(), "test-key", b"", write).unwrap()
        );
        assert_eq!(original, snapshot(&tree).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(tree.join("data"), fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(
            tree,
            materialize(cache.path(), "test-key", b"", write).unwrap()
        );
        assert_eq!(original, snapshot(&tree).unwrap());
        no_stages(cache.path());
    }

    #[test]
    fn changed_output_has_a_different_content_address() {
        let cache = temp();
        let first = materialize(cache.path(), "test-key", b"", |stage| {
            fs::write(stage.join("data"), b"first").unwrap();
            Ok(vec![])
        })
        .unwrap();
        let second = materialize(cache.path(), "test-key", b"", |stage| {
            fs::write(stage.join("data"), b"second").unwrap();
            Ok(vec![])
        })
        .unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read(first.join("data")).unwrap(), b"first");
    }

    #[test]
    fn missing_stale_malformed_and_wrong_shape_manifests_are_repaired() {
        let cache = temp();
        let tree = materialize(cache.path(), "test-key", b"", write).unwrap();
        let path = metadata_path(cache.path(), &tree);
        let expected = fs::read(&path).unwrap();
        for bytes in [b"{".as_slice(), b"{}", b"null", b"{\"format\":999}"] {
            fs::write(&path, bytes).unwrap();
            assert_eq!(
                tree,
                materialize(cache.path(), "test-key", b"", write).unwrap()
            );
            assert_eq!(fs::read(&path).unwrap(), expected);
        }
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        materialize(cache.path(), "test-key", b"", write).unwrap();
        assert_eq!(fs::read(&path).unwrap(), expected);
        fs::remove_file(&path).unwrap();
        materialize(cache.path(), "test-key", b"", write).unwrap();
        assert_eq!(fs::read(&path).unwrap(), expected);
        no_stages(cache.path());
    }

    #[test]
    fn concurrent_callbacks_for_distinct_stages_precede_publication_lock() {
        let cache = temp();
        let barrier = Arc::new(Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|index| {
                let cache = cache.path().to_owned();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    materialize(
                        &cache,
                        if index == 0 { "stage-a" } else { "stage-b" },
                        b"same",
                        |stage| {
                            let inputs = write(stage)?;
                            barrier.wait();
                            Ok(inputs)
                        },
                    )
                    .unwrap()
                })
            })
            .collect();
        let roots: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(roots[0], roots[1]);
        assert_eq!(fs::read(roots[0].join("data")).unwrap(), [0, 255, 128, 10]);
        assert_eq!(
            fs::read_dir(cache.path().join("direct/trees"))
                .unwrap()
                .count(),
            1
        );
        no_stages(cache.path());
    }

    #[test]
    fn same_stage_key_evaluations_serialize_on_the_stage_lock() {
        // Distinct repositories evaluate concurrently; one repository is
        // single-flight across processes through its deterministic stage.
        let cache = temp();
        let count = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let cache = cache.path().to_owned();
                let count = count.clone();
                std::thread::spawn(move || {
                    materialize(&cache, "single-flight", b"same", |stage| {
                        count.fetch_add(1, Ordering::SeqCst);
                        write(stage)
                    })
                    .unwrap()
                })
            })
            .collect();
        let roots: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(roots[0], roots[1]);
        assert_eq!(
            fs::read_dir(cache.path().join("direct/trees"))
                .unwrap()
                .count(),
            1
        );
        no_stages(cache.path());
    }

    #[test]
    fn retained_legacy_manifest_bytes_round_trip_without_a_symlink_field() {
        // The label-passthrough publication evidence pins a manifest written
        // before symlink entries existed; it must parse and re-serialize to
        // the identical bytes, so non-symlink identities are unchanged.
        use base64::Engine;
        let fixture: serde_json::Value = serde_json::from_slice(
            &fs::read(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/label_passthrough/native_publication_observation.json"),
            )
            .unwrap(),
        )
        .unwrap();
        let manifests = fixture["publication"]["manifests"].as_object().unwrap();
        assert!(!manifests.is_empty());
        for item in manifests.values() {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(item["raw"]["base64"].as_str().unwrap())
                .unwrap();
            let manifest: Manifest = serde_json::from_slice(&raw).unwrap();
            assert!(manifest.files.values().all(|entry| entry.symlink.is_none()));
            assert_eq!(serde_json::to_vec(&manifest).unwrap(), raw);
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_recorded_unfollowed_but_hard_links_and_special_files_are_rejected() {
        use std::os::unix::fs::symlink;
        let cache = temp();
        let outside = temp();
        fs::write(outside.path().join("precious"), b"keep").unwrap();
        let mut trees = Vec::new();
        for directory in [false, true] {
            let target = if directory {
                outside.path().to_owned()
            } else {
                outside.path().join("precious")
            };
            let tree = materialize(cache.path(), "test-key", b"", |stage| {
                symlink(&target, stage.join("link")).unwrap();
                symlink("missing/dangling", stage.join("dangling")).unwrap();
                Ok(vec![])
            })
            .unwrap();
            // Published as links (never followed, copied, or rewritten).
            assert_eq!(fs::read_link(tree.join("link")).unwrap(), target);
            assert_eq!(
                fs::read_link(tree.join("dangling")).unwrap(),
                Path::new("missing/dangling")
            );
            let manifest: Manifest =
                serde_json::from_slice(&fs::read(metadata_path(cache.path(), &tree)).unwrap())
                    .unwrap();
            assert_eq!(manifest.files["link"].symlink.as_deref(), target.to_str());
            assert!(manifest.files["link"].sha256.len() == 64);
            // Warm reuse re-verifies the same link entries.
            assert_eq!(
                tree,
                materialize(cache.path(), "test-key", b"", |stage| {
                    symlink(&target, stage.join("link")).unwrap();
                    symlink("missing/dangling", stage.join("dangling")).unwrap();
                    Ok(vec![])
                })
                .unwrap()
            );
            trees.push(tree);
        }
        // Distinct link targets are distinct content identities.
        assert_ne!(trees[0], trees[1]);
        let tree = materialize(cache.path(), "test-key", b"", |stage| {
            fs::write(stage.join("link"), b"file").unwrap();
            Ok(vec![])
        })
        .unwrap();
        assert!(!trees.contains(&tree));
        assert!(
            materialize(cache.path(), "test-key", b"", |stage| {
                fs::hard_link(outside.path().join("precious"), stage.join("hardlink")).unwrap();
                Ok(vec![])
            })
            .is_err()
        );
        assert!(
            materialize(cache.path(), "test-key", b"", |stage| {
                let _socket = std::os::unix::net::UnixListener::bind(stage.join("socket")).unwrap();
                Ok(vec![])
            })
            .is_err()
        );
        assert_eq!(fs::read(outside.path().join("precious")).unwrap(), b"keep");
        no_stages(cache.path());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_tree_and_manifest_are_quarantined_not_followed() {
        use std::os::unix::fs::symlink;
        let cache = temp();
        let outside = temp();
        fs::write(outside.path().join("precious"), b"keep").unwrap();
        let tree = materialize(cache.path(), "test-key", b"", write).unwrap();
        let path = metadata_path(cache.path(), &tree);
        fs::remove_dir_all(&tree).unwrap();
        fs::remove_file(&path).unwrap();
        symlink(outside.path(), &tree).unwrap();
        symlink(outside.path().join("precious"), &path).unwrap();
        assert_eq!(
            tree,
            materialize(cache.path(), "test-key", b"", write).unwrap()
        );
        assert!(
            !fs::symlink_metadata(&tree)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            !fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(outside.path().join("precious")).unwrap(), b"keep");
        no_stages(cache.path());
    }

    #[cfg(unix)]
    #[test]
    fn cache_ancestor_symlink_and_relative_cache_are_rejected() {
        let cache = temp();
        let outside = temp();
        std::os::unix::fs::symlink(outside.path(), cache.path().join("link")).unwrap();
        assert!(
            materialize(
                &cache.path().join("link/child"),
                "test-key",
                b"",
                |_| panic!("must not evaluate")
            )
            .is_err()
        );
        assert!(!outside.path().join("child").exists());
        assert!(
            materialize(Path::new("relative"), "test-key", b"", |_| panic!(
                "must not evaluate"
            ))
            .is_err()
        );
    }

    #[test]
    fn large_files_and_many_entries_publish_but_path_profile_holds() {
        // Above the former 16 MiB file / 64 MiB total / 10000-entry caps.
        let cache = temp();
        let tree = materialize(cache.path(), "test-key", b"", |stage| {
            for i in 0..5 {
                File::create(stage.join(i.to_string()))
                    .unwrap()
                    .set_len(17 * 1024 * 1024)
                    .unwrap();
            }
            fs::create_dir(stage.join("many")).unwrap();
            for i in 0..10_001 {
                fs::write(stage.join("many").join(i.to_string()), b"").unwrap();
            }
            Ok(vec![])
        })
        .unwrap();
        assert_eq!(
            fs::metadata(tree.join("4")).unwrap().len(),
            17 * 1024 * 1024
        );
        assert_eq!(fs::read_dir(tree.join("many")).unwrap().count(), 10_001);
        assert!(
            materialize(cache.path(), "test-key", b"", |stage| {
                let mut path = stage.to_owned();
                for _ in 0..=DEPTH_LIMIT {
                    path.push("d");
                    fs::create_dir(&path).unwrap();
                }
                Ok(vec![])
            })
            .is_err()
        );
        assert!(safe_relative(&"a".repeat(PATH_LIMIT + 1)).is_err());
        no_stages(cache.path());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_and_invalid_paths_are_rejected() {
        use std::os::unix::ffi::OsStringExt;
        let cache = temp();
        for name in [std::ffi::OsString::from_vec(vec![255]), "bad\\name".into()] {
            assert!(
                materialize(cache.path(), "test-key", b"", |stage| {
                    fs::write(stage.join(name), b"").unwrap();
                    Ok(vec![])
                })
                .is_err()
            );
        }
        no_stages(cache.path());
    }
}
