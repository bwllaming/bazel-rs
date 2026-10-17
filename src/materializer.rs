//! Verified archive repositories. This is not repository-rule execution.
//!
//! Source archives stream from the network (or `file://`) into a temporary
//! file inside the content-addressed download cache while being hashed, are
//! verified, and are published by atomic rename (`downloads/<algo>/<hex>`).
//! Extraction then streams from that blob. There are no artificial download
//! or overlay size caps.
//!
//! Registry metadata is freshly fetched under the explicit lockfile-off profile.
//! Download blobs are reused only after checking their descriptor-provided SRI.
//! Extracted trees are private staged publications with a checked file manifest.
//! The cache and its ancestors must be trusted against concurrent hostile mutation,
//! as must local execution generally; this is not a filesystem security sandbox.
use crate::extension_download::{Algorithm, Hash};
use crate::patch_inputs::LocalPatchPlan;
use crate::registry::{self, Registry};
use base64::Engine;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

const FORMAT: &str = "native-repository-v2";

#[derive(Debug)]
struct Integrity {
    algorithm: String,
    digest: Vec<u8>,
}

impl Integrity {
    fn parse(value: &str) -> Result<Self, String> {
        let (algorithm, text) = value
            .split_once('-')
            .ok_or("invalid archive integrity: expected algorithm-base64")?;
        let length = match algorithm {
            "sha1" => 20,
            "sha256" | "blake3" => 32,
            "sha384" => 48,
            "sha512" => 64,
            _ => return Err(format!("unsupported integrity algorithm {algorithm:?}")),
        };
        let decoder = base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new()
                .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
        );
        let digest = decoder
            .decode(text)
            .map_err(|e| format!("invalid integrity base64: {e}"))?;
        if digest.len() != length {
            return Err(format!(
                "invalid {algorithm} integrity length: expected {length} bytes"
            ));
        }
        Ok(Self {
            algorithm: algorithm.into(),
            digest,
        })
    }

    fn kind(&self) -> Algorithm {
        match self.algorithm.as_str() {
            "sha1" => Algorithm::Sha1,
            "sha256" => Algorithm::Sha256,
            "sha384" => Algorithm::Sha384,
            "sha512" => Algorithm::Sha512,
            "blake3" => Algorithm::Blake3,
            _ => unreachable!("validated integrity algorithm"),
        }
    }

    #[cfg(test)]
    fn verify(&self, bytes: &[u8]) -> Result<(), String> {
        self.check(self.kind().hash(bytes))
    }

    /// Verify a file, hashing it incrementally (constant memory).
    fn verify_file(&self, path: &Path) -> Result<(), String> {
        let (digest, _) = self
            .kind()
            .hash_file(path)
            .map_err(|e| format!("cannot hash {}: {e}", path.display()))?;
        self.check(digest)
    }

    fn check(&self, actual: Vec<u8>) -> Result<(), String> {
        if actual != self.digest {
            return Err(format!(
                "{} integrity mismatch: expected {}, got {}",
                self.algorithm,
                hex(&self.digest),
                hex(&actual)
            ));
        }
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn string<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("archive source requires nonempty string {key}"))
}

/// Materialize an archive without local override transformations.
pub fn materialize(
    registry: &Registry,
    name: &str,
    version: &str,
    module_file: &[u8],
    source: &Map<String, Value>,
    cache: &Path,
) -> Result<PathBuf, String> {
    materialize_with_patches(
        registry,
        name,
        version,
        module_file,
        source,
        cache,
        &LocalPatchPlan::default(),
    )
}

/// `module_file` is ORIGINAL registry content, not the patched discovery input.
pub fn materialize_with_patches(
    registry: &Registry,
    name: &str,
    version: &str,
    module_file: &[u8],
    source: &Map<String, Value>,
    cache: &Path,
    local_patches: &LocalPatchPlan,
) -> Result<PathBuf, String> {
    materialize_with_download_policy(
        registry,
        name,
        version,
        module_file,
        source,
        cache,
        local_patches,
        true,
    )
}

/// Apply repository download policy only to archive, overlay and patch acquisition.
/// Metadata/configuration reads and verified cache reuse are unaffected. Existing
/// library entrypoints retain enabled-by-default behavior.
#[allow(clippy::too_many_arguments)]
pub fn materialize_with_download_policy(
    registry: &Registry,
    name: &str,
    version: &str,
    module_file: &[u8],
    source: &Map<String, Value>,
    cache: &Path,
    local_patches: &LocalPatchPlan,
    downloads_enabled: bool,
) -> Result<PathBuf, String> {
    materialize_with_registry_config(
        registry,
        name,
        version,
        module_file,
        source,
        cache,
        local_patches,
        downloads_enabled,
        registry.read_object("bazel_registry.json")?,
    )
}

/// [`materialize_with_download_policy`] with a bazel_registry.json already
/// read through the invocation's registry session.
#[allow(clippy::too_many_arguments)]
pub fn materialize_with_registry_config(
    registry: &Registry,
    name: &str,
    version: &str,
    module_file: &[u8],
    source: &Map<String, Value>,
    cache: &Path,
    local_patches: &LocalPatchPlan,
    downloads_enabled: bool,
    config: Option<Map<String, Value>>,
) -> Result<PathBuf, String> {
    local_patches.validate_materialization()?;
    for key in source.keys() {
        if !matches!(
            key.as_str(),
            "type"
                | "url"
                | "mirror_urls"
                | "integrity"
                | "strip_prefix"
                | "archive_type"
                | "overlay"
                | "patches"
                | "patch_strip"
                // BCR documentation metadata is ignored by Bazel's archive
                // deserializer for every JSON value, never parsed or fetched.
                | "docs_url"
        ) {
            return Err(format!("unsupported archive source field {key:?}"));
        }
    }
    let patches = match source.get("patches") {
        None => Map::new(),
        Some(Value::Object(value)) => value.clone(),
        _ => return Err("archive patches must be an object".into()),
    };
    let patch_strip = match source.get("patch_strip") {
        None => 0,
        Some(value) => usize::try_from(
            value
                .as_u64()
                .ok_or("archive patch_strip must be a nonnegative integer")?,
        )
        .map_err(|_| "archive patch_strip is too large")?,
    };
    let url = string(source, "url")?;
    let integrity = Integrity::parse(string(source, "integrity")?)?;
    let prefix = match source.get("strip_prefix") {
        None => "",
        Some(Value::String(s)) => s,
        _ => return Err("archive strip_prefix must be a string".into()),
    };
    let archive_type = archive_type(source, url)?;
    let overlays = match source.get("overlay") {
        None => Map::new(),
        Some(Value::Object(o)) => o.clone(),
        _ => return Err("archive overlay must be an object".into()),
    };
    // Validate all transformations before downloading or writing anything.
    for (path, value) in &overlays {
        relative_path(path)?;
        Integrity::parse(value.as_str().ok_or("overlay integrity must be a string")?)?;
    }
    for (path, value) in &patches {
        relative_path(path).map_err(|e| format!("unsafe registry patch path: {e}"))?;
        Integrity::parse(value.as_str().ok_or("patch integrity must be a string")?)?;
    }
    let mut urls = Vec::new();
    if let Some(config) = &config
        && let Some(mirrors) = config.get("mirrors")
    {
        let mirrors = mirrors
            .as_array()
            .ok_or("registry mirrors must be an array")?;
        let source_url = reqwest::Url::parse(url).map_err(|_| "invalid archive source URL")?;
        for mirror in mirrors {
            let mirror = mirror.as_str().ok_or("registry mirror must be a string")?;
            let authority = source_url.host_str().unwrap_or("");
            let port = source_url.port().map_or(String::new(), |p| format!(":{p}"));
            let query = source_url
                .query()
                .map_or(String::new(), |q| format!("?{q}"));
            urls.push(format!(
                "{}/{authority}{port}{}{query}",
                mirror.trim_end_matches('/'),
                source_url.path()
            ));
        }
    }
    urls.push(url.to_owned());
    if let Some(mirrors) = source.get("mirror_urls") {
        for mirror in mirrors
            .as_array()
            .ok_or("archive mirror_urls must be an array")?
        {
            urls.push(
                mirror
                    .as_str()
                    .ok_or("archive mirror_urls entries must be strings")?
                    .to_owned(),
            );
        }
    }
    // URL policy is checked even when the verified blob is already cached.
    for url in &urls {
        registry::validate_download_url(url)?;
    }
    ensure_directory(cache)?;
    let cache = fs::canonicalize(cache).map_err(|e| e.to_string())?;
    let trees = cache.join("trees");
    let locks = cache.join("locks");
    ensure_directory(&trees)?;
    ensure_directory(&locks)?;
    let identity = registry_identity(
        registry,
        name,
        version,
        source,
        config.as_ref(),
        module_file,
        local_patches,
        !patches.is_empty(),
    )?;
    let key = hex(&Sha256::digest(identity));
    let lock_path = locks.join(&key);
    reject_symlink(&lock_path)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|e| format!("cannot open repository cache lock: {e}"))?;
    lock.lock_exclusive()
        .map_err(|e| format!("cannot lock repository cache: {e}"))?;
    let destination = trees.join(&key);
    let manifest_path = trees.join(format!("{key}.json"));
    reject_symlink(&destination)?;
    reject_symlink(&manifest_path)?;
    if destination.is_dir()
        && let Ok(bytes) = fs::read(&manifest_path)
        && let Ok(manifest) = serde_json::from_slice::<Manifest>(&bytes)
        && manifest.format == FORMAT
        && snapshot(&destination).is_ok_and(|actual| actual == manifest.files)
    {
        return Ok(destination);
    }
    let blob = cached_download(&cache, &urls, &integrity, downloads_enabled)?;
    let staging = tempfile::Builder::new()
        .prefix(".stage-")
        .tempdir_in(&trees)
        .map_err(|e| format!("cannot stage repository: {e}"))?;
    // Bazel names the archive by its downloaded file, the URL's last path
    // segment (e.g. in the zip link-escape error).
    let archive_name = reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| {
            parsed
                .path_segments()
                .and_then(|mut segments| segments.next_back().map(str::to_owned))
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| url.to_owned());
    crate::archive::extract_file(&blob, &archive_type, prefix, staging.path(), &archive_name)
        .map_err(|failure| match failure {
            crate::archive::Failure::PrefixNotFound(_) => {
                format!("archive strip_prefix {prefix:?} did not match")
            }
            crate::archive::Failure::Other(message) => message,
        })?;
    for (path, value) in &overlays {
        let relative = relative_path(path)?;
        let integrity = Integrity::parse(value.as_str().expect("validated overlay integrity"))?;
        let location =
            registry.download_location(&format!("modules/{name}/{version}/overlay/{path}"))?;
        let blob = cached_download(&cache, &[location], &integrity, downloads_enabled)?;
        copy_source_file(staging.path(), &relative, &blob, true)?;
    }
    // Preserve descriptor insertion order, not lexical patch-name order.
    for (path, value) in &patches {
        let integrity = Integrity::parse(value.as_str().expect("validated patch integrity"))?;
        let location =
            registry.download_location(&format!("modules/{name}/{version}/patches/{path}"))?;
        let blob = cached_download(&cache, &[location], &integrity, downloads_enabled)?;
        // PatchUtil reads the patch text into memory; that is inherent.
        let bytes = fs::read(&blob).map_err(|e| format!("cannot read registry patch: {e}"))?;
        crate::patching::parse(&bytes, patch_strip)
            .and_then(|patch| patch.apply_tree(staging.path()))
            .map_err(|e| format!("cannot apply registry patch {path:?}: {e}"))?;
    }
    // Remote patches see archive/overlay MODULE; replace it with original
    // registry bytes before local overrides, so MODULE changes apply only once.
    write_source_file(
        staging.path(),
        Path::new("MODULE.bazel"),
        module_file,
        false,
    )?;
    local_patches.apply_tree(staging.path())?;
    let manifest = Manifest {
        format: FORMAT.into(),
        files: snapshot(staging.path())?,
    };
    let encoded = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
    if destination.exists() {
        if destination.is_dir() {
            fs::remove_dir_all(&destination)
        } else {
            fs::remove_file(&destination)
        }
        .map_err(|e| format!("cannot remove corrupt repository cache: {e}"))?;
    }
    // Lock serializes publishers. A crash between these two publications leaves
    // no valid tree+manifest pair and is repaired on the next invocation.
    fs::rename(staging.path(), &destination)
        .map_err(|e| format!("cannot publish repository: {e}"))?;
    atomic_write(&manifest_path, &encoded)?;
    Ok(destination)
}

fn archive_type(source: &Map<String, Value>, url: &str) -> Result<String, String> {
    // ArchiveRepoSpecBuilder passes `type`; http_archive infers it from the
    // URL with DecompressorValue's suffix search (DecompressorValue.java:76-93).
    let kind = match source.get("archive_type") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        None | Some(Value::Null) => {
            let url = reqwest::Url::parse(url).map_err(|_| "invalid archive source URL")?;
            let path = url.path();
            const SEARCH: &[&str] = &[
                "zip", "jar", "war", "aar", "nupkg", "whl", "tar", "tar.gz", "tgz", "gz", "tar.xz",
                "txz", "xz", "tar.zst", "tzst", "zst", "tar.bz2", "tbz", "bz2", "ar", "deb", "7z",
            ];
            match SEARCH
                .iter()
                .find(|kind| path.ends_with(&format!(".{kind}")))
            {
                Some(kind) => (*kind).to_owned(),
                None => {
                    return Err(format!(
                        "cannot infer supported archive type; set archive_type to one of {}",
                        crate::archive::NATIVE_TYPES.join(", ")
                    ));
                }
            }
        }
        Some(Value::String(_)) => {
            let mut inferred = source.clone();
            inferred.remove("archive_type");
            return archive_type(&inferred, url);
        }
        _ => return Err("archive_type must be a string".into()),
    };
    match kind.as_str() {
        "zip" | "jar" | "war" | "aar" | "nupkg" | "whl" => Ok("zip".into()),
        kind if crate::archive::NATIVE_TYPES.contains(&kind) => Ok(kind.to_owned()),
        _ => Err(format!("unsupported archive type {kind:?}")),
    }
}

/// The repository download cache's blob directory for one checksum
/// algorithm, `<repository cache>/downloads/<algorithm>` (DownloadCache.java):
/// shared by module/registry archives (this module), hashed registry
/// files (crate::registry_session) and repository_ctx/module_ctx downloads
/// (crate::repository_ctx). Blobs are named by their lowercase hex digest.
pub(crate) fn download_cache_dir(repository_cache: &Path, algorithm: &str) -> PathBuf {
    repository_cache.join("downloads").join(algorithm)
}

/// `<repository cache>/downloads/<algorithm>/<hex>` ([`download_cache_dir`]).
pub(crate) fn download_cache_blob(repository_cache: &Path, algorithm: &str, hex: &str) -> PathBuf {
    download_cache_dir(repository_cache, algorithm).join(hex)
}

/// The verified content-addressed blob for `integrity`, downloading it when
/// absent or corrupt. The body streams into a temporary file in the cache
/// directory while being hashed; only a verified file is published, by an
/// atomic rename, so readers never observe a partial blob.
fn cached_download(
    cache: &Path,
    urls: &[String],
    integrity: &Integrity,
    downloads_enabled: bool,
) -> Result<PathBuf, String> {
    let directory = download_cache_dir(cache, &integrity.algorithm);
    ensure_directory(&directory)?;
    let path = directory.join(hex(&integrity.digest));
    reject_symlink(&path)?;
    if let Ok(metadata) = fs::metadata(&path)
        && metadata.is_file()
        && integrity.verify_file(&path).is_ok()
    {
        return Ok(path);
    }
    if !downloads_enabled {
        return Err("repository downloads are disabled by --repository_disable_download; no verified cached blob is available".into());
    }
    let mut failures = Vec::new();
    for url in urls {
        let mut temp = tempfile::NamedTempFile::new_in(&directory)
            .map_err(|e| format!("cannot create download file: {e}"))?;
        let mut hash = Hash::new(integrity.kind());
        let fetched = registry::download_to(
            url,
            &mut HashingWriter {
                inner: temp.as_file_mut(),
                hash: &mut hash,
            },
        )
        .and_then(|_| integrity.check(hash.finish()));
        match fetched {
            Ok(()) => {
                temp.as_file()
                    .sync_all()
                    .map_err(|e| format!("cannot sync downloaded blob: {e}"))?;
                reject_symlink(&path)?;
                temp.persist(&path)
                    .map_err(|e| format!("cannot publish cache file: {e}"))?;
                File::open(&directory)
                    .and_then(|f| f.sync_all())
                    .map_err(|e| e.to_string())?;
                return Ok(path);
            }
            Err(error) => failures.push(error),
        }
    }
    Err(format!(
        "all source downloads failed: {}",
        failures.join("; ")
    ))
}

/// A writer that hashes every byte it forwards.
struct HashingWriter<'a> {
    inner: &'a mut File,
    hash: &'a mut Hash,
}

impl std::io::Write for HashingWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(bytes)?;
        self.hash.update(&bytes[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn relative_path(path: &str) -> Result<PathBuf, String> {
    if path.is_empty()
        || path.contains(['\\', '\0', ':', '?', '#', '%'])
        || path.chars().any(char::is_control)
        || path
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
        || Path::new(path)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(format!("unsafe overlay path {path:?}"));
    }
    Ok(PathBuf::from(path))
}

fn reject_symlink(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "symlink repository cache path is unsupported: {}",
            path.display()
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "cannot inspect repository cache {}: {error}",
            path.display()
        )),
    }
}

fn ensure_directory(path: &Path) -> Result<(), String> {
    let mut current = PathBuf::new();
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err("repository cache paths containing '..' are unsupported".into());
        }
        current.push(component);
        reject_symlink(&current)?;
        match fs::create_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && current.is_dir() => {
            }
            Err(error) => {
                return Err(format!(
                    "cannot create repository directory {}: {error}",
                    current.display()
                ));
            }
        }
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    reject_symlink(path)?;
    let parent = path.parent().ok_or("cache file has no parent")?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    temp.write_all(bytes)
        .and_then(|()| temp.as_file().sync_all())
        .map_err(|e| e.to_string())?;
    temp.persist(path)
        .map_err(|e| format!("cannot publish cache file: {e}"))?;
    File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())
}

fn write_source_file(
    root: &Path,
    relative: &Path,
    bytes: &[u8],
    executable: bool,
) -> Result<(), String> {
    let path = replaceable_source_path(root, relative)?;
    fs::write(&path, bytes)
        .map_err(|e| format!("cannot write source file {}: {e}", relative.display()))?;
    source_file_mode(&path, executable)
}

/// [`write_source_file`] streaming from a verified cache blob.
fn copy_source_file(
    root: &Path,
    relative: &Path,
    blob: &Path,
    executable: bool,
) -> Result<(), String> {
    let path = replaceable_source_path(root, relative)?;
    let mut input = File::open(blob).map_err(|e| format!("cannot read cached blob: {e}"))?;
    let mut output = File::create(&path)
        .map_err(|e| format!("cannot write source file {}: {e}", relative.display()))?;
    std::io::copy(&mut input, &mut output)
        .map_err(|e| format!("cannot write source file {}: {e}", relative.display()))?;
    source_file_mode(&path, executable)
}

fn replaceable_source_path(root: &Path, relative: &Path) -> Result<PathBuf, String> {
    let path = root.join(relative);
    ensure_directory(path.parent().ok_or("source file has no parent")?)?;
    // Archive symlinks and hard links may occupy an overlay or MODULE.bazel
    // path. Replace that path itself, as Bazel's repository-cache copy does
    // (FileSystemUtils.copyFile: Files.copy REPLACE_EXISTING), rather than
    // writing through a link to another tree entry or outside the tree.
    if let Ok(metadata) = fs::symlink_metadata(&path)
        && (metadata.file_type().is_symlink() || multiply_linked(&metadata))
    {
        fs::remove_file(&path).map_err(|e| {
            format!(
                "cannot replace linked source file {}: {e}",
                relative.display()
            )
        })?;
    }
    Ok(path)
}

fn source_file_mode(path: &Path, executable: bool) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(unix)]
fn multiply_linked(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.is_file() && metadata.nlink() > 1
}

#[cfg(not(unix))]
fn multiply_linked(_metadata: &fs::Metadata) -> bool {
    false
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    format: String,
    files: BTreeMap<String, Entry>,
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    directory: bool,
    mode: u32,
    sha256: String,
    /// Literal target of a symbolic link, never followed for hashing or
    /// recursion. Omitted for files/directories so link-free manifests keep
    /// their exact pre-link bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    symlink: Option<String>,
}

fn snapshot(root: &Path) -> Result<BTreeMap<String, Entry>, String> {
    fn walk(root: &Path, dir: &Path, result: &mut BTreeMap<String, Entry>) -> Result<(), String> {
        for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            let metadata = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
            let name = path
                .strip_prefix(root)
                .map_err(|e| e.to_string())?
                .to_str()
                .ok_or("non-UTF-8 repository paths unsupported")?
                .to_owned();
            if metadata.file_type().is_symlink() {
                // Record the literal target only: no hashing, mode or recursion
                // through the link, whether it is internal, dangling or external.
                let target = fs::read_link(&path)
                    .map_err(|e| e.to_string())?
                    .into_os_string()
                    .into_string()
                    .map_err(|_| "non-UTF-8 repository symlink targets unsupported")?;
                result.insert(
                    name,
                    Entry {
                        directory: false,
                        mode: 0,
                        sha256: String::new(),
                        symlink: Some(target),
                    },
                );
                continue;
            }
            if !metadata.is_file() && !metadata.is_dir() {
                return Err("unsupported file type in repository tree".into());
            }
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o7777
            };
            #[cfg(not(unix))]
            let mode = u32::from(metadata.permissions().readonly());
            let sha256 = if metadata.is_file() {
                let mut digest = Sha256::new();
                let mut file = File::open(&path).map_err(|e| e.to_string())?;
                std::io::copy(&mut file, &mut digest).map_err(|e| e.to_string())?;
                hex(&digest.finalize())
            } else {
                String::new()
            };
            result.insert(
                name,
                Entry {
                    directory: metadata.is_dir(),
                    mode,
                    sha256,
                    symlink: None,
                },
            );
            if metadata.is_dir() {
                walk(root, &path, result)?;
            }
        }
        Ok(())
    }
    let mut result = BTreeMap::new();
    walk(root, root, &mut result)?;
    Ok(result)
}

/// The repository cache identity. Registry patches add the patch-engine marker
/// (`patching::ENGINE`); an unpatched module keeps its exact earlier identity.
#[allow(clippy::too_many_arguments)]
fn registry_identity(
    registry: &Registry,
    name: &str,
    version: &str,
    source: &Map<String, Value>,
    config: Option<&Map<String, Value>>,
    module_file: &[u8],
    local_patches: &LocalPatchPlan,
    registry_patches: bool,
) -> Result<Vec<u8>, String> {
    let base = (
        FORMAT,
        registry.to_string(),
        name,
        version,
        source,
        config,
        module_file,
        local_patches.cache_identity(),
    );
    if registry_patches {
        serde_json::to_vec(&(base, crate::patching::ENGINE))
    } else {
        serde_json::to_vec(&base)
    }
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sha2::{Sha384, Sha512};

    fn sri(bytes: &[u8]) -> String {
        format!(
            "sha256-{}",
            base64::engine::general_purpose::STANDARD.encode(Sha256::digest(bytes))
        )
    }

    fn fixture(root: &Path) -> (Registry, Map<String, Value>) {
        let registry = root.join("registry");
        fs::create_dir(&registry).unwrap();
        let mut archive = tar::Builder::new(Vec::new());
        for (name, bytes) in [
            ("pkg/BUILD.bazel", b"exports_files([\"data\"])".as_slice()),
            ("pkg/data", b"original"),
            ("pkg/MODULE.bazel", b"wrong archive module"),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive.append_data(&mut header, name, bytes).unwrap();
        }
        let archive = archive.into_inner().unwrap();
        let path = root.join("source.tar");
        fs::write(&path, &archive).unwrap();
        let source = json!({ "url": reqwest::Url::from_file_path(path).unwrap().as_str(), "integrity": sri(&archive), "strip_prefix": "pkg" }).as_object().unwrap().clone();
        (Registry::LocalPath(registry), source)
    }

    #[test]
    fn disabled_download_policy_reuses_genuine_tree_and_blob_with_exact_legacy_identity() {
        let temp = tempfile::tempdir_in("/tmp/opencode").unwrap();
        let registry = Registry::LocalPath(temp.path().join("registry"));
        fs::create_dir(registry.local_path().unwrap()).unwrap();
        let archive = include_bytes!("../tests/fixtures/build_settings/bazel-skylib-1.8.2.tar.gz");
        let module = include_bytes!(
            "../tests/fixtures/builtin_modules/registry/modules/bazel_skylib/1.8.2/MODULE.bazel"
        );
        let mut source: Map<String, Value> = serde_json::from_slice(include_bytes!(
            "../tests/fixtures/builtin_modules/registry/modules/bazel_skylib/1.8.2/source.json"
        ))
        .unwrap();
        let archive_path = temp.path().join("source.tar.gz");
        fs::write(&archive_path, archive).unwrap();
        source.insert(
            "url".into(),
            json!(
                reqwest::Url::from_file_path(&archive_path)
                    .unwrap()
                    .as_str()
            ),
        );
        let cache = temp.path().join("cache");
        let patches = LocalPatchPlan::default();
        let disabled = || {
            materialize_with_download_policy(
                &registry,
                "bazel_skylib",
                "1.8.2",
                module,
                &source,
                &cache,
                &patches,
                false,
            )
        };
        assert!(
            disabled()
                .unwrap_err()
                .contains("--repository_disable_download")
        );
        let first =
            materialize(&registry, "bazel_skylib", "1.8.2", module, &source, &cache).unwrap();
        let identity = serde_json::to_vec(&(
            FORMAT,
            registry.to_string(),
            "bazel_skylib",
            "1.8.2",
            &source,
            Option::<Value>::None,
            module.as_slice(),
            patches.cache_identity(),
        ))
        .unwrap();
        assert_eq!(
            first,
            cache.join("trees").join(hex(&Sha256::digest(identity)))
        );
        let manifest = first.with_extension("json");
        let before = fs::read(&manifest).unwrap();
        // Link-free manifests keep their pre-link serialization exactly.
        assert!(!String::from_utf8_lossy(&before).contains("symlink"));
        let blob = cache
            .join("downloads/sha256")
            .join(hex(&Sha256::digest(archive)));
        fs::remove_file(&blob).unwrap();
        assert_eq!(
            disabled().unwrap(),
            first,
            "valid tree does not need its blob"
        );
        assert_eq!(fs::read(&manifest).unwrap(), before);
        fs::remove_dir_all(&first).unwrap();
        fs::write(&blob, archive).unwrap();
        fs::remove_file(&archive_path).unwrap();
        assert_eq!(
            disabled().unwrap(),
            first,
            "verified blob can rebuild without file acquisition"
        );
        assert_eq!(fs::read(first.join("MODULE.bazel")).unwrap(), module);
        assert_eq!(fs::read(&manifest).unwrap(), before);
        fs::write(&blob, b"corrupt").unwrap();
        fs::remove_dir_all(&first).unwrap();
        assert!(
            disabled()
                .unwrap_err()
                .contains("--repository_disable_download")
        );
        assert_eq!(fs::read(blob).unwrap(), b"corrupt");
        assert!(!first.exists());
    }

    #[test]
    #[cfg(unix)]
    fn archive_links_are_recorded_literally_and_authoritative_writes_replace_links() {
        use std::os::unix::fs::{MetadataExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let registry = root_registry(temp.path());
        let mut archive = tar::Builder::new(Vec::new());
        for (name, kind, link, bytes) in [
            ("pkg/data", b'0', "", b"original".as_slice()),
            ("pkg/link", b'2', "data", b""),
            ("pkg/dangling", b'2', "missing", b""),
            ("pkg/external", b'2', "../../outside", b""),
            ("pkg/MODULE.bazel", b'2', "data", b""),
            ("pkg/hard", b'1', "pkg/data", b""),
            ("pkg/overlaid", b'1', "pkg/data", b""),
        ] {
            let mut header = tar::Header::new_ustar();
            header.set_mode(0o644);
            header.set_size(bytes.len() as u64);
            header.set_entry_type(tar::EntryType::new(kind));
            header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            header.as_mut_bytes()[157..157 + link.len()].copy_from_slice(link.as_bytes());
            header.set_cksum();
            archive.append(&header, bytes).unwrap();
        }
        let archive = archive.into_inner().unwrap();
        fs::write(temp.path().join("source.tar"), &archive).unwrap();
        let overlay = registry.local_path().unwrap().join("modules/dep/1/overlay");
        fs::create_dir_all(&overlay).unwrap();
        fs::write(overlay.join("overlaid"), b"overlay").unwrap();
        let source = json!({
            "url": reqwest::Url::from_file_path(temp.path().join("source.tar")).unwrap().as_str(),
            "integrity": sri(&archive),
            "strip_prefix": "pkg",
            "overlay": {"overlaid": sri(b"overlay")},
        })
        .as_object()
        .unwrap()
        .clone();
        let cache = temp.path().join("cache");
        let module = b"module(name='dep',version='1')\n";
        let root = materialize(&registry, "dep", "1", module, &source, &cache).unwrap();
        assert_eq!(fs::read_link(root.join("link")).unwrap(), Path::new("data"));
        assert_eq!(
            fs::read_link(root.join("external")).unwrap(),
            Path::new("../../outside")
        );
        // Authoritative MODULE and overlay writes replace links, never write through.
        assert!(
            fs::symlink_metadata(root.join("MODULE.bazel"))
                .unwrap()
                .is_file()
        );
        assert_eq!(fs::read(root.join("MODULE.bazel")).unwrap(), module);
        assert_eq!(fs::read(root.join("overlaid")).unwrap(), b"overlay");
        assert_eq!(fs::read(root.join("data")).unwrap(), b"original");
        assert_eq!(fs::metadata(root.join("data")).unwrap().nlink(), 2);
        assert_eq!(
            fs::metadata(root.join("hard")).unwrap().ino(),
            fs::metadata(root.join("data")).unwrap().ino()
        );
        let manifest_path = root.with_extension("json");
        let manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(
            manifest["files"]["link"],
            json!({"directory": false, "mode": 0, "sha256": "", "symlink": "data"})
        );
        assert_eq!(manifest["files"]["dangling"]["symlink"], "missing");
        assert_eq!(manifest["files"]["external"]["symlink"], "../../outside");
        assert!(manifest["files"]["data"].get("symlink").is_none());
        let before = fs::read(&manifest_path).unwrap();
        // Warm reuse does not follow dangling or external links.
        fs::remove_file(temp.path().join("source.tar")).unwrap();
        assert_eq!(
            materialize(&registry, "dep", "1", module, &source, &cache).unwrap(),
            root
        );
        // Retargeting to identical content, or replacing a link with a regular
        // copy of its referent, is a cache mismatch repaired from the blob.
        for tamper in [0, 1] {
            fs::remove_file(root.join("link")).unwrap();
            if tamper == 0 {
                symlink("hard", root.join("link")).unwrap();
            } else {
                fs::write(root.join("link"), b"original").unwrap();
            }
            assert_eq!(
                materialize(&registry, "dep", "1", module, &source, &cache).unwrap(),
                root
            );
            assert_eq!(fs::read_link(root.join("link")).unwrap(), Path::new("data"));
            assert_eq!(fs::read(&manifest_path).unwrap(), before);
        }
    }

    fn root_registry(root: &Path) -> Registry {
        let registry = root.join("registry");
        fs::create_dir(&registry).unwrap();
        Registry::LocalPath(registry)
    }

    #[test]
    fn documentary_source_metadata_is_inert_but_retained_in_tree_identity() {
        let temp = tempfile::Builder::new()
            .prefix("archive-docs-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        let (registry, mut source) = fixture(temp.path());
        let cache = temp.path().join("cache");
        let module = b"module(name='dep',version='1')\n";
        let first = materialize(&registry, "dep", "1", module, &source, &cache).unwrap();
        let expected = snapshot(&first).unwrap();
        let mut trees = std::collections::BTreeSet::from([first]);
        // Later metadata-only identities must reuse the already verified blob.
        fs::remove_file(temp.path().join("source.tar")).unwrap();
        for value in [
            json!("http://127.0.0.1:9/documentation"),
            json!("not a URL"),
            json!("https://synthetic-user:synthetic-password@invalid.test/docs"),
            Value::Null,
            json!(17),
            json!(true),
            json!(["file:///must-not-read", 42]),
            json!({"url": "http://127.0.0.1:9/must-not-fetch"}),
        ] {
            source.insert("docs_url".into(), value);
            let path = materialize(&registry, "dep", "1", module, &source, &cache).unwrap();
            assert_eq!(snapshot(&path).unwrap(), expected);
            assert!(
                trees.insert(path.clone()),
                "parsed metadata must remain in tree identity"
            );
            assert_eq!(
                materialize(&registry, "dep", "1", module, &source, &cache).unwrap(),
                path
            );
        }
        assert_eq!(
            fs::read_dir(cache.join("downloads/sha256"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn documentary_metadata_does_not_admit_other_source_fields_or_skip_integrity() {
        let temp = tempfile::Builder::new()
            .prefix("archive-docs-errors-")
            .tempdir_in("/tmp/opencode")
            .unwrap();
        let (registry, mut source) = fixture(temp.path());
        let cache = temp.path().join("cache");
        source.insert("docs_url".into(), json!({"anything": [false, null]}));
        source.insert("patch_cmds".into(), json!(["must not execute"]));
        let error = materialize(
            &registry,
            "dep",
            "1",
            b"module(name='dep')",
            &source,
            &cache,
        )
        .unwrap_err();
        assert!(
            error.contains("unsupported archive source field \"patch_cmds\""),
            "{error}"
        );
        assert!(!cache.exists());
        source.remove("patch_cmds");
        source.insert("integrity".into(), json!(sri(b"not the archive")));
        let error = materialize(
            &registry,
            "dep",
            "1",
            b"module(name='dep')",
            &source,
            &cache,
        )
        .unwrap_err();
        assert!(error.contains("integrity"), "{error}");
        assert_eq!(fs::read_dir(cache.join("trees")).unwrap().count(), 0);
    }

    #[test]
    fn sri_algorithms_lengths_padding_and_mismatch() {
        let bytes = b"binary\0\xff";
        for (algorithm, digest) in [
            ("sha1", sha1::Sha1::digest(bytes).to_vec()),
            ("sha256", Sha256::digest(bytes).to_vec()),
            ("sha384", Sha384::digest(bytes).to_vec()),
            ("sha512", Sha512::digest(bytes).to_vec()),
            ("blake3", blake3::hash(bytes).as_bytes().to_vec()),
        ] {
            let sri = format!(
                "{algorithm}-{}",
                base64::engine::general_purpose::STANDARD.encode(digest)
            );
            Integrity::parse(&sri).unwrap().verify(bytes).unwrap();
            Integrity::parse(sri.trim_end_matches('='))
                .unwrap()
                .verify(bytes)
                .unwrap();
            assert!(Integrity::parse(&sri).unwrap().verify(b"wrong").is_err());
        }
        for invalid in [
            "",
            "sha256-YQ==",
            "SHA256-YQ==",
            "sha256-?",
            "sha1-",
            "md5-YQ==",
        ] {
            assert!(Integrity::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn materializes_authoritative_module_and_repairs_tree_and_blob() {
        let temp = tempfile::tempdir().unwrap();
        let (registry, source) = fixture(temp.path());
        let cache = temp.path().join("cache");
        let module = b"module(name=\"dep\",version=\"1\")\n";
        let first = materialize(&registry, "dep", "1", module, &source, &cache).unwrap();
        assert_eq!(fs::read(first.join("MODULE.bazel")).unwrap(), module);
        fs::write(first.join("data"), b"corrupt").unwrap();
        fs::write(first.join("extra"), b"unexpected").unwrap();
        let integrity = Integrity::parse(source["integrity"].as_str().unwrap()).unwrap();
        let blob = cache.join("downloads/sha256").join(hex(&integrity.digest));
        fs::write(&blob, b"bad blob").unwrap();
        let second = materialize(&registry, "dep", "1", module, &source, &cache).unwrap();
        assert_eq!(first, second);
        assert_eq!(fs::read(second.join("data")).unwrap(), b"original");
        assert!(!second.join("extra").exists());
        integrity.verify(&fs::read(blob).unwrap()).unwrap();
        fs::remove_file(temp.path().join("source.tar")).unwrap();
        assert_eq!(
            materialize(&registry, "dep", "1", module, &source, &cache).unwrap(),
            first
        );
        fs::write(first.join("data"), b"corrupt again").unwrap();
        materialize(&registry, "dep", "1", module, &source, &cache).unwrap();
        assert_eq!(fs::read(first.join("data")).unwrap(), b"original");
        assert!(fs::read_dir(cache.join("trees")).unwrap().all(|e| {
            !e.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".stage-")
        }));
    }

    #[test]
    fn overlay_verified_executable_and_module_replaced_last() {
        let temp = tempfile::tempdir().unwrap();
        let (registry, mut source) = fixture(temp.path());
        let overlays = registry.local_path().unwrap().join("modules/dep/1/overlay");
        fs::create_dir_all(overlays.join("sub")).unwrap();
        fs::write(overlays.join("sub/new"), b"overlay\0").unwrap();
        fs::write(overlays.join("MODULE.bazel"), b"not authoritative").unwrap();
        source.insert(
            "overlay".into(),
            json!({"sub/new": sri(b"overlay\0"), "MODULE.bazel": sri(b"not authoritative")}),
        );
        let root = materialize(
            &registry,
            "dep",
            "1",
            b"authoritative",
            &source,
            &temp.path().join("cache"),
        )
        .unwrap();
        assert_eq!(fs::read(root.join("sub/new")).unwrap(), b"overlay\0");
        assert_eq!(
            fs::read(root.join("MODULE.bazel")).unwrap(),
            b"authoritative"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(root.join("sub/new"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o755
            );
        }
    }

    #[test]
    fn rejects_integrity_and_unsafe_transforms_without_publication() {
        let temp = tempfile::tempdir().unwrap();
        let (registry, source) = fixture(temp.path());
        let cache = temp.path().join("cache");
        for (key, value, expected) in [
            ("integrity", json!(sri(b"wrong")), "integrity mismatch"),
            (
                "patches",
                json!({"../fix.patch": sri(b"patch")}),
                "unsafe registry patch path",
            ),
            (
                "overlay",
                json!({"../outside": sri(b"bad")}),
                "unsafe overlay path",
            ),
        ] {
            let mut invalid = source.clone();
            invalid.insert(key.into(), value);
            assert!(
                materialize(&registry, "dep", "1", b"module", &invalid, &cache)
                    .unwrap_err()
                    .contains(expected)
            );
        }
        assert!(!temp.path().join("outside").exists());
        assert!(fs::read_dir(cache.join("trees")).unwrap().next().is_none());
    }

    #[test]
    fn concurrent_publishers_share_verified_tree() {
        let temp = tempfile::tempdir().unwrap();
        let (registry, source) = fixture(temp.path());
        let cache = temp.path().join("cache");
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        materialize(&registry, "dep", "1", b"module", &source, &cache).unwrap()
                    })
                })
                .collect();
            let paths: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
            assert!(paths.windows(2).all(|p| p[0] == p[1]));
        });
        assert_eq!(fs::read_dir(cache.join("trees")).unwrap().count(), 2);
    }

    fn change(path: &str, before: &str, after: &str) -> Vec<u8> {
        format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-{before}\n+{after}\n").into_bytes()
    }

    #[test]
    fn remote_patches_follow_descriptor_order_after_overlays_before_module() {
        let temp = tempfile::tempdir().unwrap();
        let (registry, mut source) = fixture(temp.path());
        let entry = registry.local_path().unwrap().join("modules/dep/1");
        fs::create_dir_all(entry.join("overlay")).unwrap();
        fs::create_dir_all(entry.join("patches")).unwrap();
        fs::write(entry.join("overlay/data"), b"overlay\n").unwrap();
        source.insert("overlay".into(), json!({"data": sri(b"overlay\n")}));
        let mut first = change("data", "overlay", "first");
        first.extend(change(
            "MODULE.bazel",
            "wrong archive module",
            "remote module",
        ));
        let second = change("data", "first", "second");
        fs::write(entry.join("patches/z-first.patch"), &first).unwrap();
        fs::write(entry.join("patches/a-second.patch"), &second).unwrap();
        // Identity of the earlier engine: the same tuple without the marker.
        let config = registry.read_object("bazel_registry.json").unwrap();
        let legacy = |source: &Map<String, Value>| {
            hex(&Sha256::digest(
                serde_json::to_vec(&(
                    FORMAT,
                    registry.to_string(),
                    "dep",
                    "1",
                    source,
                    &config,
                    b"authoritative\n".as_slice(),
                    LocalPatchPlan::default().cache_identity(),
                ))
                .unwrap(),
            ))
        };
        let cache = temp.path().join("cache");
        // An unpatched module keeps its exact earlier identity.
        let unpatched = source.clone();
        let plain = materialize(
            &registry,
            "dep",
            "1",
            b"authoritative\n",
            &unpatched,
            &cache,
        )
        .unwrap();
        assert_eq!(
            plain.file_name().unwrap().to_str().unwrap(),
            legacy(&unpatched)
        );
        let mut patches = Map::new();
        patches.insert("z-first.patch".into(), json!(sri(&first)));
        patches.insert("a-second.patch".into(), json!(sri(&second)));
        source.insert("patches".into(), Value::Object(patches));
        source.insert("patch_strip".into(), json!(1));
        let root = materialize(&registry, "dep", "1", b"authoritative\n", &source, &cache).unwrap();
        // A patched module never reuses a tree published by the old engine.
        let key = root.file_name().unwrap().to_str().unwrap().to_owned();
        assert_ne!(key, legacy(&source));
        let marked = serde_json::to_vec(&(
            (
                FORMAT,
                registry.to_string(),
                "dep",
                "1",
                &source,
                &config,
                b"authoritative\n".as_slice(),
                LocalPatchPlan::default().cache_identity(),
            ),
            crate::patching::ENGINE,
        ))
        .unwrap();
        assert_eq!(key, hex(&Sha256::digest(marked)));
        assert_eq!(fs::read(root.join("data")).unwrap(), b"second\n");
        assert_eq!(
            fs::read(root.join("MODULE.bazel")).unwrap(),
            b"authoritative\n"
        );
    }

    #[test]
    fn local_patch_snapshot_applies_once_and_changes_cache_identity() {
        use crate::bzlmod::{ModuleFile, PatchOptions};
        let temp = tempfile::tempdir().unwrap();
        let (registry, source) = fixture(temp.path());
        fs::write(temp.path().join("BUILD.bazel"), "").unwrap();
        let mut first_bytes = change("MODULE.bazel", "authoritative", "patched module");
        first_bytes.extend(change("data", "original", "first local"));
        fs::write(temp.path().join("fix.patch"), &first_bytes).unwrap();
        let options = PatchOptions {
            patches: vec!["//:fix.patch".into()],
            patch_strip: 1,
            ..Default::default()
        };
        let first_plan =
            crate::patch_inputs::snapshot(temp.path(), &ModuleFile::default(), &options).unwrap();
        let mut second_bytes = change("MODULE.bazel", "authoritative", "patched module");
        second_bytes.extend(change("data", "original", "second local"));
        fs::write(temp.path().join("fix.patch"), &second_bytes).unwrap();
        let second_plan =
            crate::patch_inputs::snapshot(temp.path(), &ModuleFile::default(), &options).unwrap();
        let cache = temp.path().join("cache");
        let first = materialize_with_patches(
            &registry,
            "dep",
            "1",
            b"authoritative\n",
            &source,
            &cache,
            &first_plan,
        )
        .unwrap();
        assert_eq!(fs::read(first.join("data")).unwrap(), b"first local\n");
        assert_eq!(
            fs::read(first.join("MODULE.bazel")).unwrap(),
            first_plan.apply_module(b"authoritative\n").unwrap()
        );
        fs::remove_file(temp.path().join("source.tar")).unwrap();
        let second = materialize_with_patches(
            &registry,
            "dep",
            "1",
            b"authoritative\n",
            &source,
            &cache,
            &second_plan,
        )
        .unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read(second.join("data")).unwrap(), b"second local\n");
        assert_eq!(
            fs::read(second.join("MODULE.bazel")).unwrap(),
            b"patched module\n"
        );
        assert_eq!(
            materialize_with_patches(
                &registry,
                "dep",
                "1",
                b"authoritative\n",
                &source,
                &cache,
                &second_plan
            )
            .unwrap(),
            second
        );
    }

    #[test]
    fn patch_failures_discard_stage_and_never_publish_a_repository() {
        let temp = tempfile::tempdir().unwrap();
        let (registry, mut source) = fixture(temp.path());
        let patches = registry.local_path().unwrap().join("modules/dep/1/patches");
        fs::create_dir_all(&patches).unwrap();
        let invalid = change("data", "context not present", "incorrect success");
        fs::write(patches.join("bad.patch"), &invalid).unwrap();
        source.insert("patches".into(), json!({"bad.patch": sri(&invalid)}));
        source.insert("patch_strip".into(), json!(1));
        let cache = temp.path().join("cache");
        assert!(
            materialize(&registry, "dep", "1", b"module", &source, &cache)
                .unwrap_err()
                .contains("cannot apply registry patch")
        );
        assert!(fs::read_dir(cache.join("trees")).unwrap().next().is_none());
        source.insert(
            "patches".into(),
            json!({"bad.patch": sri(b"different bytes")}),
        );
        assert!(
            materialize(&registry, "dep", "1", b"module", &source, &cache)
                .unwrap_err()
                .contains("integrity mismatch")
        );
        assert!(fs::read_dir(cache.join("trees")).unwrap().next().is_none());
    }

    #[test]
    fn local_commands_are_rejected_before_archive_or_cache_access() {
        use crate::bzlmod::{ModuleFile, PatchOptions};
        let temp = tempfile::tempdir().unwrap();
        let (registry, source) = fixture(temp.path());
        let marker = temp.path().join("command-ran");
        let options = PatchOptions {
            patch_cmds: vec![format!("touch {}", marker.display())],
            ..Default::default()
        };
        let plan =
            crate::patch_inputs::snapshot(temp.path(), &ModuleFile::default(), &options).unwrap();
        assert_eq!(plan.apply_module(b"module").unwrap(), b"module");
        fs::remove_file(temp.path().join("source.tar")).unwrap();
        let cache = temp.path().join("cache");
        assert!(
            materialize_with_patches(&registry, "dep", "1", b"module", &source, &cache, &plan)
                .unwrap_err()
                .contains("patch_cmds")
        );
        assert!(!cache.exists());
        assert!(!marker.exists());
    }

    #[test]
    fn reviewer_invalid_urls_rejected_identically_with_cached_blobs() {
        let temp = tempfile::tempdir().unwrap();
        let (registry, source) = fixture(temp.path());
        let warm = temp.path().join("warm");
        materialize(&registry, "dep", "1", b"module", &source, &warm).unwrap();
        for invalid_url in [
            "http://@127.0.0.1:9/source.tar",
            "http://127.0.0.1:9\\source.tar",
        ] {
            let mut invalid = source.clone();
            invalid.insert("url".into(), json!(invalid_url));
            let cold = temp.path().join("cold");
            let warm_error =
                materialize(&registry, "dep", "1", b"module", &invalid, &warm).unwrap_err();
            let cold_error =
                materialize(&registry, "dep", "1", b"module", &invalid, &cold).unwrap_err();
            assert_eq!(warm_error, cold_error);
            assert!(
                !cold.exists(),
                "URL validation must precede cache writes and downloads"
            );
        }
    }

    #[test]
    #[cfg(unix)]
    fn refuses_symlink_cache_and_repairs_mode_tampering() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let (registry, source) = fixture(temp.path());
        let cache = temp.path().join("cache");
        fs::create_dir(&cache).unwrap();
        symlink(&cache, temp.path().join("link")).unwrap();
        assert!(
            materialize(
                &registry,
                "dep",
                "1",
                b"module",
                &source,
                &temp.path().join("link")
            )
            .unwrap_err()
            .contains("symlink")
        );
        let root = materialize(&registry, "dep", "1", b"module", &source, &cache).unwrap();
        fs::set_permissions(root.join("data"), fs::Permissions::from_mode(0o755)).unwrap();
        materialize(&registry, "dep", "1", b"module", &source, &cache).unwrap();
        assert_eq!(
            fs::metadata(root.join("data"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o644
        );
    }
}
