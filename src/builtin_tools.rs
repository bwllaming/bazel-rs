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
const MODULE_SHA256: &str = "a0fab7d7a9314cfba6d73f33c50643ba26dac9be28700ff35e88306dbd69fecd";
const SOURCE_REVISION: &str = "fee2bf2d1c14bc2096068a4d46f1842472092044";

/// The official Bazel 9.3.1 distribution whose `embedded_tools/` tree is
/// compiled in: one per supported host, selected by the compile target
/// (resources/bazel_tools/<platform>/, scripts/package_builtin_tools.py).
struct Pinned {
    /// Bazel's release platform name (`bazel-9.3.1-<platform>`).
    platform: &'static str,
    distribution_url: &'static str,
    distribution_sha256: &'static str,
    file_count: usize,
    total_size: u64,
    archive: &'static [u8],
    manifest: &'static [u8],
    module: &'static [u8],
}

macro_rules! pinned {
    ($platform:literal, $sha256:literal, $files:literal, $size:literal) => {
        Pinned {
            platform: $platform,
            distribution_url: concat!(
                "https://releases.bazel.build/9.3.1/release/bazel-9.3.1-",
                $platform
            ),
            distribution_sha256: $sha256,
            file_count: $files,
            total_size: $size,
            archive: include_bytes!(concat!(
                "../resources/bazel_tools/",
                $platform,
                "/embedded_tools.tar.gz"
            )),
            manifest: include_bytes!(concat!(
                "../resources/bazel_tools/",
                $platform,
                "/manifest.json"
            )),
            module: include_bytes!(concat!(
                "../resources/bazel_tools/",
                $platform,
                "/MODULE.bazel"
            )),
        }
    };
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const PINNED: Option<Pinned> = Some(pinned!(
    "linux-x86_64",
    "7f775b54bbec487efaf0f49ccda1d2ed02cd34f582edfdd6aeeec76e39ef585c",
    421,
    73_322_435
));
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
const PINNED: Option<Pinned> = Some(pinned!(
    "linux-arm64",
    "10ba9cb76d4ee748d274dcc2981992bb12b74a203dc41a9c2eceeb8f29359366",
    420,
    71_904_827
));
/// Other compile targets have no bundle: the built-in @bazel_tools
/// repository is unavailable there (materialize fails).
#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
const PINNED: Option<Pinned> = None;

const UNSUPPORTED: &str =
    "embedded bazel_tools 9.3.1 supports only linux-x86_64 and linux-arm64 hosts";
const ARCHIVE: &[u8] = match &PINNED {
    Some(pinned) => pinned.archive,
    None => &[],
};
const MANIFEST: &[u8] = match &PINNED {
    Some(pinned) => pinned.manifest,
    None => &[],
};
#[cfg(test)]
const FILE_COUNT: usize = match &PINNED {
    Some(pinned) => pinned.file_count,
    None => 0,
};
#[cfg(test)]
const TOTAL_SIZE: u64 = match &PINNED {
    Some(pinned) => pinned.total_size,
    None => 0,
};

/// `@bazel_tools//tools/genrule:genrule-setup.sh`, the `$genrule_setup`
/// input every bash genrule sources (BazelGenRuleRule.java:31,
/// GenRuleBase.java:141-147, 170-175). Byte-identical to the embedded
/// tools entry (SHA-256 [`GENRULE_SETUP_SHA256`], checked against the
/// manifest by the tests) and to the pinned source's
/// `tools/genrule/genrule-setup.sh`.
pub const GENRULE_SETUP: &[u8] = include_bytes!("../resources/genrule-setup.sh");
pub const GENRULE_SETUP_SHA256: &str =
    "2138ccc4608e0a41291680587baced12bb80379b6924c3c4c2d9ef272f99139f";
/// Its execution path: `bazel_tools` is the canonical repository name.
pub const GENRULE_SETUP_EXEC_PATH: &str = "external/bazel_tools/tools/genrule/genrule-setup.sh";

/// The original MODULE.bazel, available without extracting the tools tree.
pub fn module_bytes() -> &'static [u8] {
    match &PINNED {
        Some(pinned) => pinned.module,
        // The platform-independent file (both bundles carry the same bytes).
        None => include_bytes!("../resources/bazel_tools/linux-x86_64/MODULE.bazel"),
    }
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
    let pinned = PINNED.as_ref().ok_or(UNSUPPORTED)?;
    if manifest.schema_version != 1
        || manifest.bazel_version != VERSION
        || manifest.platform != pinned.platform
        || manifest.source_revision != SOURCE_REVISION
        || manifest.distribution_url != pinned.distribution_url
        || manifest.distribution_sha256 != pinned.distribution_sha256
        || !valid_hash(&manifest.archive_sha256)
        || manifest.file_count != pinned.file_count
        || manifest.files.len() != pinned.file_count
        || manifest.total_size != pinned.total_size
    {
        return Err("invalid embedded bazel_tools manifest identity or totals".into());
    }
    let mut names = BTreeSet::new();
    let mut previous: Option<&str> = None;
    let mut total = 0u64;
    for entry in &manifest.files {
        let name = entry.path.as_str();
        if name.is_empty()
            || name.contains('\0')
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

/// Current compiled bundle, already staged and verified. Never creates a
/// directory, takes a publisher lock, repairs a tree or emits a live REPO use.
pub(crate) fn probe_existing(cache: &Path) -> Result<PathBuf, String> {
    if PINNED.is_none() {
        return Err(UNSUPPORTED.into());
    }
    if !cache.is_absolute() {
        return Err("embedded bazel_tools cache root must be absolute".into());
    }
    let bundle = bundle()?;
    let destination = cache.join("trees").join(&bundle.identity);
    let manifest_path = cache
        .join("trees")
        .join(format!("{}.json", bundle.identity));
    reject_symlink(&destination)?;
    reject_symlink(&manifest_path)?;
    if crate::observed_fs::read(&manifest_path).is_ok_and(|bytes| bytes == MANIFEST)
        && verified_warm(&destination, &bundle.manifest)
    {
        crate::observed_fs::proof_repository_tree(
            "builtin",
            "bazel_tools",
            &destination,
            &manifest_path,
        );
        Ok(destination)
    } else {
        Err("current built-in publication is absent or invalid".into())
    }
}

/// Publish/reuse a fully verified native tools tree under an absolute cache root.
/// The Linux x86-64 and arm64 distributions are bundled (one per binary).
pub fn materialize(cache: &Path) -> Result<PathBuf, String> {
    if PINNED.is_none() {
        return Err(UNSUPPORTED.into());
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
        && verified_warm(&destination, &bundle.manifest)
    {
        // REPO + INTEGRITY records of the analysis snapshot.
        crate::observed_fs::repository_tree(
            "builtin",
            "bazel_tools",
            &destination,
            Some(&manifest_path),
        );
        return Ok(destination);
    }
    crate::observed_fs::taint(crate::observed_fs::Taint::RepositoryMaterialized, || {
        "embedded @bazel_tools".to_owned()
    });
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
    verify_tree(staging.path(), manifest, false)?;
    Ok(staging)
}

/// The warm check of the published tree: [`verify_tree`] with each file's
/// digest from the digest memo when it records the file's path in exactly
/// its current state (recorded only when settled, repository_store's
/// Stat-mode trust), so an unchanged tree is not read; every file is read
/// under `--verify_repo_tampering` / `BAZEL_RS_VERIFY_REPOS=full`.
fn verified_warm(root: &Path, manifest: &Manifest) -> bool {
    let start = std::time::Instant::now();
    let verified = verify_tree(root, manifest, true).is_ok();
    crate::digest_memo::count_verified(crate::digest_memo::Use::Builtin, start.elapsed());
    verified
}

/// The warm check of the published `@bazel_tools` tree, as [`materialize`]
/// runs it (L2-W5g: a failed analysis-snapshot hit's healing,
/// crate::repository_store::verify_as_reused): the manifest file holds
/// exactly the embedded manifest and the tree verifies against it. Reads
/// only.
pub(crate) fn published_tree_matches(root: &Path, manifest: &Path) -> bool {
    let Ok(bundle) = bundle() else {
        return false;
    };
    fs::symlink_metadata(manifest)
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() == MANIFEST.len() as u64)
        && fs::read(manifest).is_ok_and(|bytes| bytes == MANIFEST)
        && verified_warm(root, &bundle.manifest)
}

/// Whether `root` holds exactly the manifest's files and directories, with
/// their sizes, modes and SHA-256 digests; `memo` takes the digests through
/// the digest memo (the warm check of the published tree).
fn verify_tree(root: &Path, manifest: &Manifest, memo: bool) -> Result<(), String> {
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
            let mismatch = || format!("embedded bazel_tools cache integrity mismatch: {name}");
            if memo {
                // At most one byte beyond the expected size, as below.
                let sha256 = crate::digest_memo::file_sha256(
                    &path,
                    &metadata,
                    crate::digest_memo::Use::Builtin,
                    |file| {
                        let (sha256, copied) = crate::digest_memo::sha256_reader(
                            &mut file.take(expected.size + 1),
                            expected.size,
                        )?;
                        if copied == expected.size {
                            Ok(sha256)
                        } else {
                            Err(std::io::Error::other("size changed while reading"))
                        }
                    },
                )
                .map_err(|_| mismatch())?;
                if crate::digest_memo::hex(&sha256) != expected.sha256 {
                    return Err(mismatch());
                }
            } else {
                let mut digest = Sha256::new();
                let file = File::open(&path).map_err(|error| error.to_string())?;
                let copied = std::io::copy(&mut file.take(expected.size + 1), &mut digest)
                    .map_err(|error| error.to_string())?;
                if copied != expected.size || hex(&digest.finalize()) != expected.sha256 {
                    return Err(mismatch());
                }
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
                // Never follows symlinks inside the corrupt tree, and makes
                // read-only directories (tampering) writable first.
                crate::repository_ctx::remove_tree(path)
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
    fn current_builtin_proof_never_stages_or_repairs() {
        let temp = tempfile::tempdir_in(crate::test_temp::root()).unwrap();
        let cache = temp.path().join("builtins");
        assert!(probe_existing(&cache).is_err());
        assert!(!cache.exists());
        let root = materialize(&cache).unwrap();
        assert_eq!(probe_existing(&cache).unwrap(), root);
        fs::write(root.join("MODULE.bazel"), "tampered").unwrap();
        assert!(probe_existing(&cache).is_err());
        assert_eq!(fs::read(root.join("MODULE.bazel")).unwrap(), b"tampered");
    }

    #[test]
    fn genrule_setup_is_the_embedded_tools_entry() {
        let manifest: Manifest = serde_json::from_slice(MANIFEST).unwrap();
        let entry = manifest
            .files
            .iter()
            .find(|entry| entry.path == "tools/genrule/genrule-setup.sh")
            .unwrap();
        assert_eq!(entry.sha256, GENRULE_SETUP_SHA256);
        assert_eq!(entry.size, GENRULE_SETUP.len() as u64);
        assert_eq!(hex(&Sha256::digest(GENRULE_SETUP)), GENRULE_SETUP_SHA256);
        assert_eq!(
            GENRULE_SETUP_EXEC_PATH,
            format!("external/bazel_tools/{}", entry.path)
        );
    }

    /// Both bundled platforms' manifests (whichever one this binary embeds):
    /// same release, same platform-independent Starlark/MODULE files; only
    /// the compiled helpers and the JDK differ.
    #[test]
    fn every_bundled_platform_manifest_matches_its_pins() {
        let manifests: [(&str, &str, usize, u64, &[u8]); 2] = [
            (
                "linux-x86_64",
                "7f775b54bbec487efaf0f49ccda1d2ed02cd34f582edfdd6aeeec76e39ef585c",
                421,
                73_322_435,
                include_bytes!("../resources/bazel_tools/linux-x86_64/manifest.json"),
            ),
            (
                "linux-arm64",
                "10ba9cb76d4ee748d274dcc2981992bb12b74a203dc41a9c2eceeb8f29359366",
                420,
                71_904_827,
                include_bytes!("../resources/bazel_tools/linux-arm64/manifest.json"),
            ),
        ];
        let parsed: Vec<Manifest> = manifests
            .iter()
            .map(|(platform, sha256, files, size, bytes)| {
                let manifest: Manifest = serde_json::from_slice(bytes).unwrap();
                assert_eq!(manifest.platform, *platform);
                assert_eq!(manifest.distribution_sha256, *sha256);
                assert_eq!(
                    manifest.distribution_url,
                    format!("https://releases.bazel.build/9.3.1/release/bazel-9.3.1-{platform}")
                );
                assert_eq!(manifest.source_revision, SOURCE_REVISION);
                assert_eq!(manifest.bazel_version, VERSION);
                assert_eq!((manifest.file_count, manifest.total_size), (*files, *size));
                manifest
            })
            .collect();
        let digests = |manifest: &Manifest| -> BTreeMap<String, String> {
            manifest
                .files
                .iter()
                .map(|entry| (entry.path.clone(), entry.sha256.clone()))
                .collect()
        };
        let (x86, arm) = (digests(&parsed[0]), digests(&parsed[1]));
        let mut differing: Vec<&str> = x86
            .iter()
            .filter(|(path, sha256)| arm.get(*path) != Some(*sha256))
            .map(|(path, _)| path.as_str())
            .filter(|path| !path.starts_with("jdk/"))
            .collect();
        differing.sort_unstable();
        assert_eq!(
            differing,
            [
                "tools/cpp/modules_tools/aggregate-ddi",
                "tools/cpp/modules_tools/generate-modmap",
                "tools/zip/zipper/zipper",
            ]
        );
        assert!(arm.keys().all(|path| x86.contains_key(path)));
        assert_eq!(x86["MODULE.bazel"], MODULE_SHA256);
        assert_eq!(arm["MODULE.bazel"], MODULE_SHA256);
    }

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

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
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
        // What a failed analysis-snapshot hit's check names first (L2-W5h).
        let published = crate::repository_store::PublishedTree {
            kind: "builtin",
            id: bundle.identity.clone(),
            root: root.clone(),
            manifest: manifest_path.clone(),
        };
        let named = || {
            let mismatch = crate::repository_store::verify_as_reused(&published).unwrap_err();
            (mismatch.class, mismatch.path)
        };
        crate::repository_store::verify_as_reused(&published).unwrap();

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
        assert_eq!(named(), ("manifest", manifest_path.clone()));
        assert_eq!(materialize(&cache).unwrap(), root);
        assert_eq!(fs::read(&module).unwrap(), module_bytes());

        // Even an unchanged, valid cached manifest cannot attest altered content.
        fs::write(&module, &corrupt).unwrap();
        assert_eq!(named(), ("content", module.clone()));
        materialize(&cache).unwrap();
        assert_eq!(fs::read(&module).unwrap(), module_bytes());

        fs::remove_file(&module).unwrap();
        fs::write(root.join("unexpected"), b"extra").unwrap();
        fs::create_dir(root.join("unexpected-empty-directory")).unwrap();
        assert_eq!(named(), ("missing", module.clone()));
        materialize(&cache).unwrap();
        assert!(!root.join("unexpected").exists());
        assert!(!root.join("unexpected-empty-directory").exists());
        assert_eq!(fs::read(&module).unwrap(), module_bytes());

        fs::set_permissions(&module, fs::Permissions::from_mode(0o644)).unwrap();
        fs::set_permissions(root.join("tools"), fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(named(), ("mode", module.clone()));
        materialize(&cache).unwrap();
        assert_eq!(mode(&fs::metadata(&module).unwrap()), 0o755);
        assert_eq!(mode(&fs::metadata(root.join("tools")).unwrap()), 0o755);

        fs::write(&manifest_path, b"malformed cached manifest").unwrap();
        assert_eq!(named(), ("manifest", manifest_path.clone()));
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

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
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

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
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
        // The warm (memo) check and the staging check agree.
        let rejected = |stage: &Path| {
            let (warm, staged) = (
                verify_tree(stage, &manifest, true),
                verify_tree(stage, &manifest, false),
            );
            assert_eq!(warm.is_err(), staged.is_err());
            warm.is_err()
        };
        assert!(!rejected(stage.path()));
        fs::create_dir(stage.path().join("empty")).unwrap();
        assert!(rejected(stage.path()));
        fs::remove_dir(stage.path().join("empty")).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(rejected(stage.path()));
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(stage.path().join("dir"), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(rejected(stage.path()));
        set_directory_mode(&stage.path().join("dir")).unwrap();
        fs::write(&file, b"binary\0\xfe").unwrap();
        assert!(rejected(stage.path()));
        fs::write(&file, b"wrong size").unwrap();
        assert!(rejected(stage.path()));
        fs::remove_file(&file).unwrap();
        assert!(rejected(stage.path()));
        drop(stage);

        assert!(stage_tree(temp.path(), b"not a gzip archive", &manifest).is_err());
        manifest.files[0].sha256 = "0".repeat(64);
        assert!(stage_tree(temp.path(), &archive, &manifest).is_err());
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
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
