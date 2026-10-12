//! Verified, bounded archive repositories. This is not repository-rule execution.
//!
//! Registry metadata is freshly fetched under the explicit lockfile-off profile.
//! Download blobs are reused only after checking their descriptor-provided SRI.
//! Extracted trees are private staged publications with a checked file manifest.
//! The cache and its ancestors must be trusted against concurrent hostile mutation,
//! as must local execution generally; this is not a filesystem security sandbox.
use crate::registry::{self, Registry};
use base64::Engine;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256, Sha384, Sha512};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

const DOWNLOAD_LIMIT: u64 = 256 * 1024 * 1024;
const OVERLAY_LIMIT: u64 = 16 * 1024 * 1024;
const FORMAT: &str = "native-repository-v1";

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

    fn verify(&self, bytes: &[u8]) -> Result<(), String> {
        let actual = match self.algorithm.as_str() {
            "sha1" => sha1::Sha1::digest(bytes).to_vec(),
            "sha256" => Sha256::digest(bytes).to_vec(),
            "sha384" => Sha384::digest(bytes).to_vec(),
            "sha512" => Sha512::digest(bytes).to_vec(),
            "blake3" => blake3::hash(bytes).as_bytes().to_vec(),
            _ => unreachable!("validated integrity algorithm"),
        };
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

/// Called only for the selected registry and only when its source root is needed.
pub fn materialize(
    registry: &Registry,
    name: &str,
    version: &str,
    module_file: &[u8],
    source: &Map<String, Value>,
    cache: &Path,
) -> Result<PathBuf, String> {
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
        ) {
            return Err(format!("unsupported archive source field {key:?}"));
        }
    }
    if let Some(patches) = source.get("patches") {
        let patches = patches
            .as_object()
            .ok_or("archive patches must be an object")?;
        if !patches.is_empty() {
            return Err("registry archive patches are not implemented".into());
        }
    }
    if let Some(strip) = source.get("patch_strip")
        && strip.as_u64().is_none()
    {
        return Err("archive patch_strip must be a nonnegative integer".into());
    }
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
    let mut urls = Vec::new();
    let config = registry.read_object("bazel_registry.json")?;
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
    let identity = serde_json::to_vec(&(
        FORMAT,
        registry.to_string(),
        name,
        version,
        source,
        config,
        module_file,
    ))
    .map_err(|e| e.to_string())?;
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
    let bytes = cached_download(&cache, &urls, &integrity, DOWNLOAD_LIMIT)?;
    let staging = tempfile::Builder::new()
        .prefix(".stage-")
        .tempdir_in(&trees)
        .map_err(|e| format!("cannot stage repository: {e}"))?;
    crate::archive::extract(&bytes, &archive_type, prefix, staging.path())?;
    for (path, value) in &overlays {
        let relative = relative_path(path)?;
        let integrity = Integrity::parse(value.as_str().expect("validated overlay integrity"))?;
        let location =
            registry.download_location(&format!("modules/{name}/{version}/overlay/{path}"))?;
        let bytes = cached_download(&cache, &[location], &integrity, OVERLAY_LIMIT)?;
        write_source_file(staging.path(), &relative, &bytes, true)?;
    }
    // Registry MODULE is authoritative even when archive/overlay contains another.
    write_source_file(
        staging.path(),
        Path::new("MODULE.bazel"),
        module_file,
        false,
    )?;
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
    let kind = match source.get("archive_type") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        None | Some(Value::Null) => {
            let url = reqwest::Url::parse(url).map_err(|_| "invalid archive source URL")?;
            let path = url.path();
            if path.ends_with(".tar.gz") || path.ends_with(".tgz") {
                "tar.gz".into()
            } else if path.ends_with(".tar") {
                "tar".into()
            } else if path.ends_with(".zip")
                || path.ends_with(".jar")
                || path.ends_with(".war")
                || path.ends_with(".aar")
            {
                "zip".into()
            } else {
                return Err(
                    "cannot infer supported archive type; set archive_type to tar, tar.gz or zip"
                        .into(),
                );
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
        "tar" | "tar.gz" | "tgz" => Ok(kind),
        "zip" | "jar" | "war" | "aar" => Ok("zip".into()),
        _ => Err(format!("unsupported archive type {kind:?}")),
    }
}

fn cached_download(
    cache: &Path,
    urls: &[String],
    integrity: &Integrity,
    limit: u64,
) -> Result<Vec<u8>, String> {
    let directory = cache.join("downloads").join(&integrity.algorithm);
    ensure_directory(&directory)?;
    let path = directory.join(hex(&integrity.digest));
    reject_symlink(&path)?;
    if let Ok(metadata) = fs::metadata(&path)
        && metadata.is_file()
        && metadata.len() <= limit
        && let Ok(bytes) = fs::read(&path)
        && integrity.verify(&bytes).is_ok()
    {
        return Ok(bytes);
    }
    let mut failures = Vec::new();
    for url in urls {
        let fetched = registry::download(url, limit).and_then(|bytes| {
            integrity.verify(&bytes)?;
            Ok(bytes)
        });
        match fetched {
            Ok(bytes) => {
                atomic_write(&path, &bytes)?;
                return Ok(bytes);
            }
            Err(error) => failures.push(error),
        }
    }
    Err(format!(
        "all source downloads failed: {}",
        failures.join("; ")
    ))
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
    let path = root.join(relative);
    ensure_directory(path.parent().ok_or("source file has no parent")?)?;
    reject_symlink(&path)?;
    fs::write(&path, bytes)
        .map_err(|e| format!("cannot write source file {}: {e}", relative.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            &path,
            fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
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
}

fn snapshot(root: &Path) -> Result<BTreeMap<String, Entry>, String> {
    fn walk(root: &Path, dir: &Path, result: &mut BTreeMap<String, Entry>) -> Result<(), String> {
        for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
            let path = entry.map_err(|e| e.to_string())?.path();
            let metadata = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
            if !metadata.is_file() && !metadata.is_dir() {
                return Err("unsupported file type in repository tree".into());
            }
            let name = path
                .strip_prefix(root)
                .map_err(|e| e.to_string())?
                .to_str()
                .ok_or("non-UTF-8 repository paths unsupported")?
                .to_owned();
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn rejects_integrity_patches_and_unsafe_overlays_without_publication() {
        let temp = tempfile::tempdir().unwrap();
        let (registry, source) = fixture(temp.path());
        let cache = temp.path().join("cache");
        for (key, value, expected) in [
            ("integrity", json!(sri(b"wrong")), "integrity mismatch"),
            (
                "patches",
                json!({"fix.patch": sri(b"patch")}),
                "patches are not implemented",
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
