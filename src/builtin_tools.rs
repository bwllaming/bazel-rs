//! Genuine, compile-time embedded Bazel tools; no download or process execution.
//!
//! Every demand verifies the entire tree against the immutable embedded manifest,
//! never against a manifest inferred from mutable cache contents. As with the other
//! repository caches, cache ancestors must be trusted against concurrent hostile
//! mutation: this is not a filesystem sandbox.

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuiltinId {
    BazelTools93,
}

pub const VERSION: &str = crate::COMPATIBILITY_VERSION;
const ARCHIVE: &[u8] = include_bytes!("../resources/bazel_tools/embedded_tools.tar.gz");
const MANIFEST: &[u8] = include_bytes!("../resources/bazel_tools/manifest.json");
const MODULE_SHA256: &str = "a0fab7d7a9314cfba6d73f33c50643ba26dac9be28700ff35e88306dbd69fecd";
const FILE_COUNT: usize = 421;
const TOTAL_SIZE: u64 = 73_326_972;

/// The original MODULE.bazel, available without extracting the tools tree.
pub fn module_bytes() -> &'static [u8] {
    include_bytes!("../resources/bazel_tools/MODULE.bazel")
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    bazel_version: String,
    platform: String,
    source_revision: String,
    distribution_url: String,
    distribution_sha256: String,
    archive_sha256: String,
    file_count: usize,
    total_size: u64,
    files: Vec<Entry>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: String,
    size: u64,
    sha256: String,
    mode: u32,
}

struct Bundle {
    manifest: Manifest,
    identity: String,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_manifest(manifest: &Manifest) -> Result<(), String> {
    if manifest.schema_version != 1
        || manifest.bazel_version != VERSION
        || manifest.platform != "linux-x86_64"
        || manifest.source_revision != "e89755babf3e941bc1e9d8120b1cd48854ab9dfa"
        || manifest.distribution_url
            != "https://releases.bazel.build/9.3.0/release/bazel-9.3.0-linux-x86_64"
        || manifest.distribution_sha256
            != "d302d22ed77ee5658b87857249baa579db84300ecebe0a1e9a357b5b20f80eb8"
        || !valid_hash(&manifest.archive_sha256)
        || manifest.file_count != FILE_COUNT
        || manifest.files.len() != FILE_COUNT
        || manifest.total_size != TOTAL_SIZE
    {
        return Err("invalid embedded bazel_tools manifest identity or totals".into());
    }
    let mut names = BTreeSet::new();
    let mut previous: Option<&str> = None;
    let mut total = 0u64;
    for entry in &manifest.files {
        let name = entry.path.as_str();
        if name.is_empty()
            || name.contains(['\\', '\0', ':'])
            || name.split('/').any(|part| matches!(part, "" | "." | ".."))
            || previous.is_some_and(|previous| previous >= name)
            || !valid_hash(&entry.sha256)
            || entry.mode != 0o755
        {
            return Err(format!(
                "invalid embedded bazel_tools manifest entry: {name:?}"
            ));
        }
        total = total
            .checked_add(entry.size)
            .ok_or("embedded bazel_tools manifest size overflow")?;
        names.insert(name);
        previous = Some(name);
    }
    for name in &names {
        for (index, _) in name.match_indices('/') {
            if names.contains(&name[..index]) {
                return Err(format!(
                    "embedded bazel_tools file/directory conflict: {name}"
                ));
            }
        }
    }
    if total != manifest.total_size {
        return Err("embedded bazel_tools manifest file sizes do not match total".into());
    }
    let module = manifest
        .files
        .iter()
        .find(|entry| entry.path == "MODULE.bazel")
        .ok_or("embedded bazel_tools manifest is missing MODULE.bazel")?;
    if module.sha256 != MODULE_SHA256
        || module.size != module_bytes().len() as u64
        || hex(&Sha256::digest(module_bytes())) != MODULE_SHA256
    {
        return Err("embedded bazel_tools MODULE.bazel integrity mismatch".into());
    }
    Ok(())
}

fn bundle() -> Result<&'static Bundle, String> {
    static BUNDLE: OnceLock<Result<Bundle, String>> = OnceLock::new();
    BUNDLE
        .get_or_init(|| {
            let manifest: Manifest = serde_json::from_slice(MANIFEST)
                .map_err(|error| format!("invalid embedded bazel_tools manifest: {error}"))?;
            validate_manifest(&manifest)?;
            if hex(&Sha256::digest(ARCHIVE)) != manifest.archive_sha256 {
                return Err("embedded bazel_tools archive SHA-256 mismatch".into());
            }
            // Bind both the exact immutable manifest and the verified archive.
            let mut digest = Sha256::new();
            digest.update(b"native-embedded-bazel-tools-v1\0");
            digest.update(MANIFEST);
            digest.update(manifest.archive_sha256.as_bytes());
            Ok(Bundle {
                manifest,
                identity: hex(&digest.finalize()),
            })
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// Content identity anchored in the verified compiled-in archive and manifest.
pub fn bundle_identity() -> Result<String, String> {
    Ok(bundle()?.identity.clone())
}

/// Publish/reuse a fully verified native tools tree under an absolute cache root.
/// Only the Linux x86-64 distribution is currently bundled.
pub fn materialize(cache: &Path) -> Result<PathBuf, String> {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return Err("embedded bazel_tools 9.3.0 supports only linux-x86_64".into());
    }
    if !cache.is_absolute() {
        return Err("embedded bazel_tools cache root must be absolute".into());
    }
    let bundle = bundle()?;
    ensure_directory(cache)?;
    let cache = fs::canonicalize(cache).map_err(|error| error.to_string())?;
    let trees = cache.join("trees");
    let locks = cache.join("locks");
    ensure_directory(&trees)?;
    ensure_directory(&locks)?;
    let lock_path = locks.join(&bundle.identity);
    reject_symlink(&lock_path)?;
    if let Ok(metadata) = fs::symlink_metadata(&lock_path)
        && !metadata.is_file()
    {
        return Err("embedded bazel_tools cache lock is not a regular file".into());
    }
    // Persistent lock files: unlinking them would let publishers lock different inodes.
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| format!("open embedded bazel_tools cache lock: {error}"))?;
    lock.lock_exclusive()
        .map_err(|error| format!("lock embedded bazel_tools cache: {error}"))?;
    let destination = trees.join(&bundle.identity);
    let manifest_path = trees.join(format!("{}.json", bundle.identity));
    reject_symlink(&destination)?;
    reject_symlink(&manifest_path)?;
    if fs::symlink_metadata(&manifest_path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() == MANIFEST.len() as u64)
        && fs::read(&manifest_path).is_ok_and(|bytes| bytes == MANIFEST)
        && verify_tree(&destination, &bundle.manifest).is_ok()
    {
        return Ok(destination);
    }
    let staging = stage_tree(&trees, ARCHIVE, &bundle.manifest)?;
    remove_cache_entry(&destination)?;
    fs::rename(staging.path(), &destination)
        .map_err(|error| format!("publish embedded bazel_tools tree: {error}"))?;
    // An interrupted tree/manifest publication is detected and repaired next demand.
    let mut sidecar = tempfile::NamedTempFile::new_in(&trees)
        .map_err(|error| format!("stage embedded bazel_tools manifest: {error}"))?;
    sidecar
        .write_all(MANIFEST)
        .and_then(|()| sidecar.as_file().sync_all())
        .map_err(|error| format!("write embedded bazel_tools manifest: {error}"))?;
    reject_symlink(&manifest_path)?;
    if fs::symlink_metadata(&manifest_path).is_ok_and(|metadata| metadata.is_dir()) {
        remove_cache_entry(&manifest_path)?;
    }
    sidecar
        .persist(&manifest_path)
        .map_err(|error| format!("publish embedded bazel_tools manifest: {error}"))?;
    File::open(&trees)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync embedded bazel_tools cache: {error}"))?;
    Ok(destination)
}

fn stage_tree(
    trees: &Path,
    archive: &[u8],
    manifest: &Manifest,
) -> Result<tempfile::TempDir, String> {
    let staging = tempfile::Builder::new()
        .prefix(".stage-")
        .tempdir_in(trees)
        .map_err(|error| format!("stage embedded bazel_tools: {error}"))?;
    set_directory_mode(staging.path())?;
    crate::archive::extract(archive, "tar.gz", "", staging.path())?;
    verify_tree(staging.path(), manifest)?;
    Ok(staging)
}

fn verify_tree(root: &Path, manifest: &Manifest) -> Result<(), String> {
    let files: BTreeMap<_, _> = manifest
        .files
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let mut directories = BTreeSet::new();
    for name in files.keys() {
        for (index, _) in name.match_indices('/') {
            directories.insert(&name[..index]);
        }
    }
    let mut pending = vec![root.to_path_buf()];
    let mut file_count = 0;
    let mut directory_count = 0;
    while let Some(directory) = pending.pop() {
        let metadata = fs::symlink_metadata(&directory).map_err(|error| error.to_string())?;
        if !metadata.is_dir() || mode(&metadata) != 0o755 {
            return Err(format!(
                "invalid embedded bazel_tools directory: {}",
                directory.display()
            ));
        }
        for child in fs::read_dir(&directory).map_err(|error| error.to_string())? {
            let path = child.map_err(|error| error.to_string())?.path();
            let name = path
                .strip_prefix(root)
                .map_err(|error| error.to_string())?
                .to_str()
                .ok_or("non-UTF-8 embedded bazel_tools cache path")?;
            let metadata = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
            if metadata.is_dir() && directories.contains(name) {
                directory_count += 1;
                pending.push(path);
                continue;
            }
            let expected = files
                .get(name)
                .ok_or_else(|| format!("unexpected embedded bazel_tools cache entry: {name}"))?;
            if !metadata.is_file()
                || metadata.len() != expected.size
                || mode(&metadata) != expected.mode
            {
                return Err(format!("invalid embedded bazel_tools cache file: {name}"));
            }
            let mut digest = Sha256::new();
            let file = File::open(&path).map_err(|error| error.to_string())?;
            let copied = std::io::copy(&mut file.take(expected.size + 1), &mut digest)
                .map_err(|error| error.to_string())?;
            if copied != expected.size || hex(&digest.finalize()) != expected.sha256 {
                return Err(format!(
                    "embedded bazel_tools cache integrity mismatch: {name}"
                ));
            }
            file_count += 1;
        }
    }
    if file_count != files.len() || directory_count != directories.len() {
        return Err("embedded bazel_tools cache has missing entries".into());
    }
    Ok(())
}

fn mode(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        0
    }
}

fn set_directory_mode(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .map_err(|error| error.to_string())?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "symlink embedded bazel_tools cache path is unsupported: {}",
            path.display()
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("inspect embedded bazel_tools cache: {error}")),
    }
}

fn ensure_directory(path: &Path) -> Result<(), String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err("embedded bazel_tools cache paths containing '..' are unsupported".into());
        }
        current.push(component);
        reject_symlink(&current)?;
        match fs::create_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && current.is_dir() => {
            }
            Err(error) => {
                return Err(format!(
                    "create embedded bazel_tools cache directory: {error}"
                ));
            }
        }
    }
    Ok(())
}

fn remove_cache_entry(path: &Path) -> Result<(), String> {
    reject_symlink(path)?;
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.is_dir() {
                // remove_dir_all does not follow symlinks inside the corrupt tree.
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            }
            .map_err(|error| format!("remove corrupt embedded bazel_tools cache entry: {error}"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "inspect corrupt embedded bazel_tools cache entry: {error}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_rejects_paths_duplicates_conflicts_hashes_modes_and_totals() {
        let valid: Manifest = serde_json::from_slice(MANIFEST).unwrap();
        validate_manifest(&valid).unwrap();
        for path in [
            "",
            "/abs",
            "../escape",
            "a/../b",
            "a//b",
            "./a",
            "a/",
            "a\\b",
            "C:a",
            "a\0b",
        ] {
            let mut invalid = valid.clone();
            invalid.files[0].path = path.into();
            assert!(validate_manifest(&invalid).is_err(), "accepted {path:?}");
        }
        let mut invalid = valid.clone();
        invalid.files[1].path = invalid.files[0].path.clone();
        assert!(validate_manifest(&invalid).is_err());
        let mut invalid = valid.clone();
        invalid.files[1].path = format!("{}/child", invalid.files[0].path);
        assert!(validate_manifest(&invalid).is_err());
        for hash in ["a", &"G".repeat(64)] {
            let mut invalid = valid.clone();
            invalid.files[0].sha256 = hash.into();
            assert!(validate_manifest(&invalid).is_err());
        }
        for mode in [0o644, 0o4755, 0o777] {
            let mut invalid = valid.clone();
            invalid.files[0].mode = mode;
            assert!(validate_manifest(&invalid).is_err());
        }
        let mut invalid = valid.clone();
        invalid.file_count -= 1;
        assert!(validate_manifest(&invalid).is_err());
        let mut invalid = valid.clone();
        invalid.total_size -= 1;
        assert!(validate_manifest(&invalid).is_err());
        let mut invalid = valid.clone();
        invalid.files[0].size += 1;
        assert!(validate_manifest(&invalid).is_err());
        let mut invalid = valid;
        invalid.files[0].size = u64::MAX;
        assert!(validate_manifest(&invalid).is_err());
    }

    #[test]
    fn relative_cache_is_rejected_without_writes() {
        assert!(
            materialize(Path::new("relative-cache"))
                .unwrap_err()
                .contains("absolute")
        );
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn genuine_bundle_cold_warm_and_corruption_repair() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("builtins");
        let root = materialize(&cache).unwrap();
        let bundle = bundle().unwrap();
        assert_eq!(bundle.manifest.file_count, FILE_COUNT);
        assert_eq!(bundle.manifest.total_size, TOTAL_SIZE);
        assert_eq!(root, cache.join("trees").join(bundle_identity().unwrap()));
        assert_eq!(fs::read(root.join("MODULE.bazel")).unwrap(), module_bytes());
        assert_eq!(materialize(&cache).unwrap(), root);
        let module = root.join("MODULE.bazel");
        let manifest_path = root.with_extension("json");

        // Same-size corruption plus a forged cache manifest must never be trusted.
        let mut corrupt = module_bytes().to_vec();
        corrupt[0] ^= 1;
        fs::write(&module, &corrupt).unwrap();
        let mut forged: serde_json::Value = serde_json::from_slice(MANIFEST).unwrap();
        for entry in forged["files"].as_array_mut().unwrap() {
            if entry["path"] == "MODULE.bazel" {
                entry["sha256"] = hex(&Sha256::digest(&corrupt)).into();
            }
        }
        fs::write(&manifest_path, serde_json::to_vec(&forged).unwrap()).unwrap();
        assert_eq!(materialize(&cache).unwrap(), root);
        assert_eq!(fs::read(&module).unwrap(), module_bytes());

        // Even an unchanged, valid cached manifest cannot attest altered content.
        fs::write(&module, &corrupt).unwrap();
        materialize(&cache).unwrap();
        assert_eq!(fs::read(&module).unwrap(), module_bytes());

        fs::remove_file(&module).unwrap();
        fs::write(root.join("unexpected"), b"extra").unwrap();
        fs::create_dir(root.join("unexpected-empty-directory")).unwrap();
        materialize(&cache).unwrap();
        assert!(!root.join("unexpected").exists());
        assert!(!root.join("unexpected-empty-directory").exists());
        assert_eq!(fs::read(&module).unwrap(), module_bytes());

        fs::set_permissions(&module, fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(root.join("tools"), fs::Permissions::from_mode(0o700)).unwrap();
        materialize(&cache).unwrap();
        assert_eq!(mode(&fs::metadata(&module).unwrap()), 0o755);
        assert_eq!(mode(&fs::metadata(root.join("tools")).unwrap()), 0o755);

        fs::write(&manifest_path, b"malformed cached manifest").unwrap();
        materialize(&cache).unwrap();
        assert_eq!(fs::read(&manifest_path).unwrap(), MANIFEST);
        fs::remove_file(&manifest_path).unwrap();
        materialize(&cache).unwrap();
        assert_eq!(fs::read_dir(cache.join("trees")).unwrap().count(), 2);
        assert_eq!(fs::read_dir(cache.join("locks")).unwrap().count(), 1);
        eprintln!(
            "verified genuine bazel_tools: {FILE_COUNT} files, {TOTAL_SIZE} bytes, identity {}",
            bundle.identity
        );
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn concurrent_publishers_share_one_verified_tree() {
        let temp = tempfile::tempdir().unwrap();
        let cache = temp.path().join("builtins");
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| scope.spawn(|| materialize(&cache).unwrap()))
                .collect();
            let paths: Vec<_> = handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect();
            assert!(paths.windows(2).all(|pair| pair[0] == pair[1]));
        });
        assert_eq!(fs::read_dir(cache.join("trees")).unwrap().count(), 2);
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn synthetic_tree_exactness_and_failed_stages_are_cleaned() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let data = b"binary\0\xff";
        let mut manifest: Manifest = serde_json::from_slice(MANIFEST).unwrap();
        manifest.files = vec![Entry {
            path: "dir/file".into(),
            size: data.len() as u64,
            sha256: hex(&Sha256::digest(data)),
            mode: 0o755,
        }];
        manifest.file_count = 1;
        manifest.total_size = data.len() as u64;
        let mut tar = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_ustar();
        header.set_size(data.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, "dir/file", &data[..]).unwrap();
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gzip.write_all(&tar.into_inner().unwrap()).unwrap();
        let archive = gzip.finish().unwrap();
        let stage = stage_tree(temp.path(), &archive, &manifest).unwrap();
        let file = stage.path().join("dir/file");
        fs::create_dir(stage.path().join("empty")).unwrap();
        assert!(verify_tree(stage.path(), &manifest).is_err());
        fs::remove_dir(stage.path().join("empty")).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(verify_tree(stage.path(), &manifest).is_err());
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(stage.path().join("dir"), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(verify_tree(stage.path(), &manifest).is_err());
        set_directory_mode(&stage.path().join("dir")).unwrap();
        fs::write(&file, b"binary\0\xfe").unwrap();
        assert!(verify_tree(stage.path(), &manifest).is_err());
        fs::write(&file, b"wrong size").unwrap();
        assert!(verify_tree(stage.path(), &manifest).is_err());
        fs::remove_file(&file).unwrap();
        assert!(verify_tree(stage.path(), &manifest).is_err());
        drop(stage);

        assert!(stage_tree(temp.path(), b"not a gzip archive", &manifest).is_err());
        manifest.files[0].sha256 = "0".repeat(64);
        assert!(stage_tree(temp.path(), &archive, &manifest).is_err());
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn hostile_cache_symlinks_never_mutate_external_targets() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let outside = temp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        let sentinel = outside.join("sentinel");
        fs::write(&sentinel, b"untouched").unwrap();
        let linked_cache = temp.path().join("linked");
        symlink(&outside, &linked_cache).unwrap();
        assert!(materialize(&linked_cache).unwrap_err().contains("symlink"));

        let cache = temp.path().join("builtins");
        let root = materialize(&cache).unwrap();
        let manifest_path = root.with_extension("json");
        fs::remove_file(&manifest_path).unwrap();
        symlink(&sentinel, &manifest_path).unwrap();
        assert!(materialize(&cache).unwrap_err().contains("symlink"));
        fs::remove_file(&manifest_path).unwrap();
        fs::write(&manifest_path, MANIFEST).unwrap();
        fs::remove_file(root.join("MODULE.bazel")).unwrap();
        symlink(&sentinel, root.join("MODULE.bazel")).unwrap();
        symlink(&outside, root.join("external-directory")).unwrap();
        materialize(&cache).unwrap();
        assert!(!root.join("external-directory").exists());
        assert_eq!(fs::read(root.join("MODULE.bazel")).unwrap(), module_bytes());
        fs::remove_dir_all(&root).unwrap();
        symlink(&outside, &root).unwrap();
        assert!(materialize(&cache).unwrap_err().contains("symlink"));
        fs::remove_file(&root).unwrap();
        let lock = cache.join("locks").join(bundle_identity().unwrap());
        fs::remove_file(&lock).unwrap();
        symlink(&sentinel, &lock).unwrap();
        assert!(materialize(&cache).unwrap_err().contains("symlink"));
        assert_eq!(fs::read(&sentinel).unwrap(), b"untouched");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    }
}
