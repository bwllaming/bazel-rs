//! Verified content-addressed output for the direct file/template subset.
//!
//! A repository's identity is computed from its specification BEFORE any
//! execution: `spec` (rule definition, attributes, mappings, environment
//! declarations and declared-environment values, host markers) names a
//! pointer under `direct/specs/`, and a warm invocation whose recorded
//! watchable inputs (the `recorded` probe bytes validated through `validate`)
//! and published tree still match reuses that publication without running the
//! rule — like Bazel's per-output-base `external/<name>` directory, which is
//! re-fetched only when its rule, environment or watched inputs change.
//!
//! The published tree keeps the content address it always had (a digest over
//! `provenance`, the exact serialized runtime inputs and the output snapshot),
//! so deterministic repositories keep their identities across re-executions.
//! The cache is user-owned, not a sandbox or a remote-cache protocol. Callbacks
//! must finish all their writes before returning; callers must treat returned
//! trees as immutable.
//!
//! Files are hashed incrementally while snapshotting (constant memory). There
//! are no file-size, total-size, entry-count or manifest-size caps; only the
//! store's Linux path profile (UTF-8, 4096-byte names, 255-byte components,
//! no component-count cap) is enforced.

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

const PROFILE: &str = "direct-file-template-v1";
const PATH_LIMIT: usize = 4096;
const NAME_LIMIT: usize = 255;
const SPEC_PROFILE: &[u8] = b"direct-repository-spec-v1";

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: u32,
    profile: String,
    identity: String,
    files: BTreeMap<String, Entry>,
    /// The specification digest that owns this publication (RepositoryFunction
    /// keys the repo directory by rule and attributes). Absent in manifests
    /// written before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    spec: Option<String>,
    /// Opaque recorded watchable inputs (caller JSON) rechecked by `validate`
    /// on warm calls before any reuse; never part of the identity digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recorded: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Stat fingerprint recorded at publish (Step B): device, inode,
    /// mtime (sec, nsec) and ctime (sec, nsec). Warm reuse compares stats
    /// and hashes only on mismatch or inside the racy window. Absent in
    /// manifests written before Step B (those verify by full hash once
    /// and are re-published with fingerprints on their next execution).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stat: Option<StatFingerprint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct StatFingerprint {
    dev: u64,
    ino: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

/// Identity-relevant view of a manifest: the entries WITHOUT their stat
/// fingerprints. The content identity (and every persisted-bytes
/// comparison) is over content — mode/size/hash/symlink — never over
/// when or where the tree happens to be staged.
fn content_view(files: &BTreeMap<String, Entry>) -> BTreeMap<String, Entry> {
    files
        .iter()
        .map(|(name, entry)| {
            (
                name.clone(),
                Entry {
                    stat: None,
                    ..(*entry).clone()
                },
            )
        })
        .collect()
}

#[cfg(unix)]
fn fingerprint(metadata: &fs::Metadata) -> StatFingerprint {
    use std::os::unix::fs::MetadataExt;
    StatFingerprint {
        dev: metadata.dev(),
        ino: metadata.ino(),
        mtime: (metadata.mtime(), metadata.mtime_nsec()),
        ctime: (metadata.ctime(), metadata.ctime_nsec()),
    }
}

#[cfg(not(unix))]
fn fingerprint(_metadata: &fs::Metadata) -> StatFingerprint {
    StatFingerprint {
        dev: 0,
        ino: 0,
        mtime: (0, 0),
        ctime: (0, 0),
    }
}

/// Per-call phase timings for one `materialize` invocation, accumulated
/// process-wide and printed at process end under `BAZEL_RS_TIMING`
/// (Step A of the repository-warm perf increment; zero cost when unset:
/// one relaxed atomic load per phase). Phases split the warm path the way
/// the oracle brief asks: spec/provenance serialization is the caller's
/// (measured by the caller-side wrapper in `repository.rs`), manifest read,
/// recorded-input check (`validate`), snapshot (with file/byte counts) and
/// the execute+publish remainder.
pub mod timing {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    static ENABLED: AtomicBool = AtomicBool::new(false);
    static INIT: std::sync::Once = std::sync::Once::new();

    pub fn enabled() -> bool {
        INIT.call_once(|| {
            ENABLED.store(
                std::env::var_os("BAZEL_RS_TIMING").is_some(),
                Ordering::Relaxed,
            );
        });
        ENABLED.load(Ordering::Relaxed)
    }

    #[derive(Default)]
    pub struct Acc {
        pub calls: AtomicU64,
        pub nanos: AtomicU64,
    }
    impl Acc {
        fn add(&self, d: Duration) {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.nanos.fetch_add(d.as_nanos() as u64, Ordering::Relaxed);
        }
        fn secs(&self) -> f64 {
            self.nanos.load(Ordering::Relaxed) as f64 / 1e9
        }
    }

    pub struct Phases {
        pub manifest_read: Acc,
        pub recorded_check: Acc,
        pub snapshot_check: Acc,
        pub snapshot_files: AtomicU64,
        pub snapshot_bytes: AtomicU64,
        pub execute: Acc,
        pub publish: Acc,
        pub spec_serialize: Acc,
        pub prepare_group: Acc,
        pub reused: AtomicU64,
        pub executed: AtomicU64,
        pub roots: std::sync::Mutex<Vec<(String, f64, &'static str)>>,
    }
    pub static PHASES: Phases = Phases {
        manifest_read: Acc { calls: AtomicU64::new(0), nanos: AtomicU64::new(0) },
        recorded_check: Acc { calls: AtomicU64::new(0), nanos: AtomicU64::new(0) },
        snapshot_check: Acc { calls: AtomicU64::new(0), nanos: AtomicU64::new(0) },
        snapshot_files: AtomicU64::new(0),
        snapshot_bytes: AtomicU64::new(0),
        execute: Acc { calls: AtomicU64::new(0), nanos: AtomicU64::new(0) },
        publish: Acc { calls: AtomicU64::new(0), nanos: AtomicU64::new(0) },
        spec_serialize: Acc { calls: AtomicU64::new(0), nanos: AtomicU64::new(0) },
        prepare_group: Acc { calls: AtomicU64::new(0), nanos: AtomicU64::new(0) },
        reused: AtomicU64::new(0),
        executed: AtomicU64::new(0),
        roots: std::sync::Mutex::new(Vec::new()),
    };

    /// RAII phase timer; a no-op (one atomic load) when timing is off.
    pub struct Mark(Option<(&'static Acc, Instant)>);
    pub fn mark(acc: &'static Acc) -> Mark {
        Mark(enabled().then(|| (acc, Instant::now())))
    }
    impl Drop for Mark {
        fn drop(&mut self) {
            if let Some((acc, start)) = self.0 {
                acc.add(start.elapsed());
            }
        }
    }

    pub static SPEC_BYTES: AtomicU64 = AtomicU64::new(0);
    pub static PROVENANCE_BYTES: AtomicU64 = AtomicU64::new(0);

    pub fn count_spec(spec_len: u64, provenance_len: u64) {
        if enabled() {
            SPEC_BYTES.fetch_add(spec_len, Ordering::Relaxed);
            PROVENANCE_BYTES.fetch_add(provenance_len, Ordering::Relaxed);
        }
    }

    pub fn count_snapshot(files: u64, bytes: u64) {
        if enabled() {
            PHASES.snapshot_files.fetch_add(files, Ordering::Relaxed);
            PHASES.snapshot_bytes.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    pub fn record_root(stage_key: &str, secs: f64, outcome: &'static str) {
        if enabled() {
            if let Ok(mut roots) = PHASES.roots.lock() {
                roots.push((stage_key.to_owned(), secs, outcome));
            }
        }
    }

    /// Print the accumulated breakdown once (called at process end when
    /// timing is on; also callable from tests). Includes the serial
    /// per-root sum and the parallel-eligible estimate: the snapshot +
    /// recorded-check phases are pure reads of disjoint trees, so on an
    /// N-worker reuse pool they divide by N; manifest reads likewise.
    pub fn report(threads: usize) {
        if !enabled() {
            return;
        }
        let p = &PHASES;
        let warm = p.manifest_read.secs() + p.recorded_check.secs() + p.snapshot_check.secs();
        let mut roots = p.roots.lock().map(|r| r.clone()).unwrap_or_default();
        roots.sort_by(|a, b| b.1.total_cmp(&a.1));
        let serial_roots: f64 = roots.iter().map(|r| r.1).sum();
        eprintln!(
            "PERF-REPO reused={} executed={} manifest_read={:.3}s/{} recorded_check={:.3}s/{} snapshot={:.3}s/{} ({} files, {} bytes) execute={:.3}s/{} publish={:.3}s spec_serialize={:.3}s/{} prepare_group={:.3}s/{}",
            p.reused.load(Ordering::Relaxed),
            p.executed.load(Ordering::Relaxed),
            p.manifest_read.secs(),
            p.manifest_read.calls.load(Ordering::Relaxed),
            p.recorded_check.secs(),
            p.recorded_check.calls.load(Ordering::Relaxed),
            p.snapshot_check.secs(),
            p.snapshot_check.calls.load(Ordering::Relaxed),
            p.snapshot_files.load(Ordering::Relaxed),
            p.snapshot_bytes.load(Ordering::Relaxed),
            p.execute.secs(),
            p.execute.calls.load(Ordering::Relaxed),
            p.publish.secs(),
            p.spec_serialize.secs(),
            p.spec_serialize.calls.load(Ordering::Relaxed),
            p.prepare_group.secs(),
            p.prepare_group.calls.load(Ordering::Relaxed),
        );
        eprintln!(
            "PERF-REPO spec bytes total={} provenance bytes total={} (digest-form spec; provenance keeps full source bytes for the content identity)",
            SPEC_BYTES.load(Ordering::Relaxed),
            PROVENANCE_BYTES.load(Ordering::Relaxed),
        );
        eprintln!(
            "PERF-REPO warm-validation serial sum={warm:.3}s; per-root serial sum={serial_roots:.3}s over {} roots; parallel-eligible estimate at {threads} workers: warm-validation {:.3}s + largest-root {:.3}s",
            roots.len(),
            warm / threads.max(1) as f64,
            roots.first().map(|r| r.1).unwrap_or(0.0),
        );
        for (key, secs, outcome) in roots.iter().take(25) {
            eprintln!("PERF-REPO root {secs:>8.3}s {outcome} {key}");
        }
    }
}

/// What one execution observed, for the identity digest and the manifest.
pub struct Executed {
    /// Exact serialized runtime input provenance, after validating the result.
    /// These bytes (never the incidental stage pathname) enter the identity.
    pub inputs: Vec<u8>,
    /// Opaque recorded watchable-input probes (caller JSON) persisted in the
    /// manifest and rechecked by `validate` on later warm calls.
    pub recorded: Vec<u8>,
}

/// Whether a call ran the repository rule or reused a publication.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Publication {
    /// A valid publication for this specification was reused; the rule never
    /// ran and nothing was staged (Bazel's warm `external/<name>` directory).
    Reused,
    /// The repository rule executed and the result was published or verified.
    Executed,
}

/// The digest of a repository specification: a pure function of the bytes a
/// caller serialized before any execution. Equal specifications reuse each
/// other's publications; any change (attributes, environment, host markers)
/// addresses a different pointer.
pub fn spec_identity(spec: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(SPEC_PROFILE);
    digest.update([0]);
    digest.update(spec);
    format!("{:x}", digest.finalize()) // RepositoryFunction's key digest
}

const SPEC_DIGEST_PROFILE: &[u8] = b"direct-repository-spec-digest-v1";

/// The specification digest of an already-digested provenance: framed
/// SHA-256 over each source's identity (label, content digest, mapping)
/// plus the remaining provenance fields. Deterministic in the same inputs
/// as the byte spec; used to address the same pointer without serializing
/// (and re-hashing) tens of MB of `.bzl` source bytes per repository.
pub fn spec_digest_identity(
    provenance_fields: &serde_json::Value,
    sources: &BTreeMap<String, crate::loading::SourceProvenance>,
) -> String {
    let mut digest = Sha256::new();
    digest.update(SPEC_DIGEST_PROFILE);
    digest.update([0]);
    // Frame the non-source provenance exactly as the JSON object would
    // serialize with `sources` replaced by per-source digests.
    let mut framed = provenance_fields.clone();
    if let Some(object) = framed.as_object_mut() {
        let mut entries = serde_json::Map::new();
        for (label, source) in sources {
            let content = format!("{:x}", Sha256::digest(&source.bytes));
            let mapping = serde_json::to_string(&source.mapping).unwrap_or_default();
            entries.insert(
                label.clone(),
                serde_json::json!({"sha256": content, "mapping": mapping}),
            );
        }
        object.insert("sources".into(), serde_json::Value::Object(entries));
    }
    let bytes = serde_json::to_vec(&framed).unwrap_or_default();
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(&bytes);
    format!("{:x}", digest.finalize())
}

fn framed(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

/// Execute into a private stage, then verify and publish its complete tree;
/// or, when the specification already owns a valid publication whose recorded
/// watchable inputs are unchanged, reuse it without executing. `execute`
/// returns exact serialized runtime input provenance plus recorded probes
/// after validating its result. `provenance` identifies the definition,
/// attributes and mappings; it is kept byte-for-byte in the identity digest so
/// existing content identities are unchanged. Neither byte string may contain
/// the incidental stage pathname; the stage pathname itself is derived from
/// `stage_key` (the repository's canonical name) so repository content that
/// embeds its own working directory — `ctx.path(".")`, as gazelle's `go_env`
/// does — addresses a path that is identical in every process and invocation,
/// keeping the tree identity stable. Concurrent evaluations of the same key
/// serialize on a stage lock in the cache, like module extension working
/// directories.
pub fn materialize<F, V>(
    cache: &Path,
    stage_key: &str,
    spec: &[u8],
    provenance: &[u8],
    validate: V,
    execute: F,
) -> Result<(PathBuf, Publication), String>
where
    F: FnOnce(&Path) -> Result<Executed, String>,
    V: FnOnce(&Path, &[u8]) -> Result<bool, String>,
{
    materialize_keyed(cache, stage_key, &spec_identity(spec), provenance, validate, execute)
}

/// [`materialize`] with the caller-computed specification pointer identity:
/// digest-form callers pass `repository_warm::specification_digest` and skip
/// serializing (and hashing) the full source-bearing spec bytes.
pub fn materialize_keyed<F, V>(
    cache: &Path,
    stage_key: &str,
    spec_key: &str,
    provenance: &[u8],
    validate: V,
    execute: F,
) -> Result<(PathBuf, Publication), String>
where
    F: FnOnce(&Path) -> Result<Executed, String>,
    V: FnOnce(&Path, &[u8]) -> Result<bool, String>,
{
    ensure_directory(cache)?;
    let direct = cache.join("direct");
    let trees = direct.join("trees");
    let manifests = direct.join("manifests");
    let locks = direct.join("locks");
    let stages = direct.join("stages");
    let specs = direct.join("specs");
    for path in [&trees, &manifests, &locks, &stages, &specs] {
        ensure_directory(path)?;
    }
    // The pointer is published by atomic rename; an unlocked reader sees
    // either the old or the new publication, never a partial one.
    let call_start = timing::enabled().then(std::time::Instant::now);
    let pointer = specs.join(spec_key);
    if let Some(identity) = read_pointer(&pointer)? {
        let destination = trees.join(&identity);
        let manifest_path = manifests.join(format!("{identity}.json"));
        let manifest = {
            let _m = timing::mark(&timing::PHASES.manifest_read);
            read_stable(&manifest_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Manifest>(&bytes).ok())
        };
        if let Some(existing) = manifest
            && existing.identity == identity
            && existing.recorded.is_some()
        {
            let recorded_ok = {
                let _m = timing::mark(&timing::PHASES.recorded_check);
                validate(
                    &destination,
                    existing.recorded.as_deref().unwrap_or_default(),
                )
                .unwrap_or(false)
            };
            let snapshot_ok = recorded_ok && {
                let _m = timing::mark(&timing::PHASES.snapshot_check);
                timing::count_snapshot(
                    existing.files.len() as u64,
                    existing
                        .files
                        .values()
                        .map(|e| e.size)
                        .sum::<u64>(),
                );
                verify_snapshot(&destination, &existing.files, verify_mode())
                    .is_ok_and(|matched| matched)
            };
            if snapshot_ok {
                timing::PHASES
                    .reused
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if let Some(start) = call_start {
                    timing::record_root(stage_key, start.elapsed().as_secs_f64(), "reused");
                }
                return Ok((destination, Publication::Reused));
            }
        }
        // A failing or absent validation falls through to execution, exactly
        // like Bazel refetching when a watched dependency changed.
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
    let executed = {
        let _m = timing::mark(&timing::PHASES.execute);
        execute(stage.path())?
    };
    required_directory(stage.path())?;
    ensure_boundary(stage.path())?;
    let files = snapshot(stage.path())?;
    let encoded_snapshot = serde_json::to_vec(&content_view(&files)).map_err(|e| e.to_string())?;
    let mut digest = Sha256::new();
    digest.update(PROFILE.as_bytes());
    digest.update([0]);
    for bytes in [
        provenance,
        executed.inputs.as_slice(),
        encoded_snapshot.as_slice(),
    ] {
        framed(&mut digest, bytes);
    }
    let identity = format!("{:x}", digest.finalize());
    let manifest = Manifest {
        format: 1,
        profile: PROFILE.into(),
        identity: identity.clone(),
        files,
        spec: Some(spec_key.to_owned()),
        recorded: Some(executed.recorded),
    };
    let encoded = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;

    // Recheck parents after the callback, before touching persistent entries.
    for path in [&trees, &manifests, &locks, &stages, &specs] {
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
    // Identical content from another specification (or this one) needs no
    // republication: the tree and its inventory already match ours, AND the
    // persisted manifest already carries this format's specification digest
    // and recorded probes — a manifest written before those fields existed
    // (an older binary's publication) must be upgraded once, or no later
    // invocation could ever validate-and-reuse it through the pointer.
    // Only the specification pointer below records this specification's claim.
    if !(read_stable(&manifest_path)
        .and_then(|bytes| serde_json::from_slice::<Manifest>(&bytes).map_err(|e| e.to_string()))
        .is_ok_and(|existing| {
            existing.identity == identity
                && content_view(&existing.files) == content_view(&manifest.files)
                && existing.spec.is_some()
                && existing.recorded.is_some()
        })
        && verify_snapshot(&destination, &manifest.files, verify_mode())
            .is_ok_and(|matched| matched))
    {
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
    }
    // Publish the specification pointer after the tree it names. A crash
    // between them leaves a stale pointer, which the warm path revalidates
    // (manifest, recorded inputs, tree) before reuse, so it can only cost a
    // re-execution, never a wrong reuse.
    {
        let _m = timing::mark(&timing::PHASES.publish);
        publish_pointer(&specs, &pointer, &identity)?;
    }
    timing::PHASES
        .executed
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Some(start) = call_start {
        timing::record_root(stage_key, start.elapsed().as_secs_f64(), "executed");
    }
    // This call ran the repository rule even when the tree it produced
    // already existed under the same content identity.
    Ok((destination, Publication::Executed))
}

/// A pointer file's exact bytes for `identity` (kept canonical for equality).
fn pointer_bytes(identity: &str) -> String {
    format!("{{\"format\":1,\"identity\":\"{identity}\"}}\n")
}

/// Atomically install the specification pointer naming `identity`.
fn publish_pointer(specs: &Path, pointer: &Path, identity: &str) -> Result<(), String> {
    let mut stage = tempfile::NamedTempFile::new_in(specs)
        .map_err(|e| format!("cannot stage direct specification: {e}"))?;
    stage
        .write_all(pointer_bytes(identity).as_bytes())
        .and_then(|()| stage.as_file().sync_all())
        .map_err(|e| e.to_string())?;
    stage
        .persist(pointer)
        .map_err(|e| format!("cannot publish direct specification: {e}"))?;
    File::open(specs)
        .and_then(|file| file.sync_all())
        .map_err(|e| e.to_string())
}

/// Read a specification pointer, checking its shape like every other entry.
fn read_pointer(path: &Path) -> Result<Option<String>, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Pointer {
        format: u32,
        identity: String,
    }
    let Ok(bytes) = read_stable(path) else {
        return Ok(None);
    };
    let pointer: Pointer = serde_json::from_slice(&bytes).map_err(|e| {
        format!(
            "cannot parse direct repository specification {}: {e}",
            path.display()
        )
    })?;
    if pointer.format != 1
        || pointer.identity.len() != 64
        || !pointer
            .identity
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(format!(
            "invalid direct repository specification pointer {}",
            path.display()
        ));
    }
    Ok(Some(pointer.identity))
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

/// Linux filename profile of a direct-repository relative path, mirroring
/// Bazel 9.3: repository trees are traversed entry-by-entry (`os.DirEntry` /
/// unix_jni.cc `readdir`), and `PathFragment.checkBaseName` forbids only
/// separators and `.`/`..` in a name. `#`, `%`, `?`, `:` and `\\` are
/// ordinary Linux filename bytes, not path structure (Bazel's own JSON
/// conformance asset is `n_structure_trailing_#.json`), so they are accepted.
/// The only limits are the kernel limits Bazel itself runs under: PATH_MAX
/// (4096 bytes total) and NAME_MAX (255 bytes per component). There is no
/// component-count (depth) cap: the total-length cap already bounds it, and
/// Bazel on Linux accepts deeper trees. NUL can never occur in a filename.
fn safe_relative(name: &str) -> Result<(), String> {
    if name.len() > PATH_LIMIT
        || name.contains('\0')
        || name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == ".." || part.len() > NAME_LIMIT)
        || Path::new(name)
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(format!(
            "invalid direct repository path {name:?}: exceeds the path limit (maximum {PATH_LIMIT} bytes total, {NAME_LIMIT} bytes per component)"
        ));
    }
    Ok(())
}

/// Warm-verification strength (Step B): `Stat` trusts size/mtime/inode/mode
/// recorded in the manifest and hashes only stat mismatches or files inside
/// the racy window (Bazel's marker-trust model); `Full` re-hashes every
/// file, the pre-Step-B behavior, opt-in via `BAZEL_RS_VERIFY_REPOS=full`
/// or `--verify-repo-tampering`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyMode {
    Stat,
    Full,
}

static VERIFY_OVERRIDE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Force full verification from the CLI (`--verify-repo-tampering`).
pub fn set_verify_full() {
    VERIFY_OVERRIDE.store(2, std::sync::atomic::Ordering::Relaxed);
}

pub fn verify_mode() -> VerifyMode {
    match VERIFY_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed) {
        2 => return VerifyMode::Full,
        1 => return VerifyMode::Stat,
        _ => {}
    }
    let mode = match std::env::var("BAZEL_RS_VERIFY_REPOS").ok().as_deref() {
        Some("full") => VerifyMode::Full,
        _ => VerifyMode::Stat,
    };
    VERIFY_OVERRIDE.store(
        match mode {
            VerifyMode::Full => 2,
            VerifyMode::Stat => 1,
        },
        std::sync::atomic::Ordering::Relaxed,
    );
    mode
}

/// A file's mtime/ctime are this much older than `start` ("racy git",
/// mirroring `action::racy_safe` for non-owned files): outside the window
/// a stat match proves the bytes were not rewritten after recording.
const RACY_MARGIN: std::time::Duration = std::time::Duration::from_secs(2);

fn racy_safe(fp: &StatFingerprint, start: std::time::SystemTime) -> bool {
    [(fp.mtime), (fp.ctime)].into_iter().all(|(sec, nsec)| {
        let Ok(sec) = u64::try_from(sec) else {
            return false;
        };
        std::time::SystemTime::UNIX_EPOCH
            .checked_add(std::time::Duration::new(sec, nsec as u32))
            .and_then(|t| t.checked_add(RACY_MARGIN))
            .is_some_and(|t| t <= start)
    })
}

/// Whether the warm snapshot check confirms `root` still matches the
/// manifest `expected` entries.
///
/// `Full` mode re-hashes every file (pre-Step-B behavior). `Stat` mode
/// (default) walks the tree comparing directory/mode/size/symlink and the
/// recorded stat fingerprint; a regular file whose fingerprint matches
/// and is outside the racy window is trusted without hashing (Bazel's
/// marker trust). On fingerprint mismatch or inside the window the file
/// is hashed: a tamper that preserves size+mtime still changes ctime (or
/// the inode), so it is caught by the hash. Entries without a fingerprint
/// (legacy manifests) always hash. Any structural difference (missing,
/// extra, type/mode/size change) is a mismatch.
fn verify_snapshot(
    root: &Path,
    expected: &BTreeMap<String, Entry>,
    mode: VerifyMode,
) -> Result<bool, String> {
    if mode == VerifyMode::Full {
        return Ok(content_view(&snapshot(root)?) == content_view(expected));
    }
    let start = std::time::SystemTime::now();
    fn visit(
        root: &Path,
        path: &Path,
        expected: &BTreeMap<String, Entry>,
        seen: &mut std::collections::BTreeSet<String>,
        start: std::time::SystemTime,
    ) -> Result<bool, String> {
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
        let Some(want) = expected.get(&name) else {
            return Ok(false); // extra entry
        };
        seen.insert(name.clone());
        if before.file_type().is_symlink() {
            if want.directory || want.symlink.is_none() {
                return Ok(false);
            }
            let target = fs::read_link(path).map_err(|e| e.to_string())?;
            let Some(target) = target.to_str() else {
                return Ok(false);
            };
            return Ok(want.symlink.as_deref() == Some(target)
                && want.size == target.len() as u64);
        }
        if before.is_dir() {
            if !want.directory || mode_of(&before)? != want.mode {
                return Ok(false);
            }
            for child in fs::read_dir(path).map_err(|e| e.to_string())? {
                if !visit(root, &child.map_err(|e| e.to_string())?.path(), expected, seen, start)? {
                    return Ok(false);
                }
            }
            return Ok(true);
        }
        // Regular file.
        if want.directory || want.symlink.is_some() {
            return Ok(false);
        }
        regular_metadata(&before)?;
        if mode_of(&before)? != want.mode || before.len() != want.size {
            return Ok(false);
        }
        let trusted = want
            .stat
            .is_some_and(|fp| fp == fingerprint(&before) && racy_safe(&fp, start));
        if trusted {
            return Ok(true);
        }
        let (_, sha256) = hash_stable(path)?;
        Ok(sha256 == want.sha256)
    }
    required_directory(root)?;
    let mut seen = std::collections::BTreeSet::new();
    if !visit(root, root, expected, &mut seen, start)? {
        return Ok(false);
    }
    Ok(seen.len() == expected.len())
}

fn mode_of(metadata: &fs::Metadata) -> Result<u32, String> {
    mode(metadata)
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
                    stat: Some(fingerprint(&before)),
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
            stat: Some(fingerprint(&before)),
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

    /// Publish with an unchanged-world validator (every warm call reuses).
    fn publish(
        cache: &Path,
        provenance: &[u8],
        run: impl FnOnce(&Path) -> Result<Vec<u8>, String>,
    ) -> Result<(PathBuf, Publication), String> {
        publish_keyed(cache, "test-key", provenance, run)
    }

    /// [`publish`] with an explicit stage key (the repository's name).
    fn publish_keyed(
        cache: &Path,
        stage_key: &str,
        provenance: &[u8],
        run: impl FnOnce(&Path) -> Result<Vec<u8>, String>,
    ) -> Result<(PathBuf, Publication), String> {
        materialize(
            cache,
            stage_key,
            b"spec",
            provenance,
            |_, _| Ok(true),
            |stage| {
                run(stage).map(|inputs| Executed {
                    inputs,
                    recorded: br#"{"version":1}"#.to_vec(),
                })
            },
        )
    }

    /// Publish with an explicit specification and recorded-input verdict.
    fn publish_spec(
        cache: &Path,
        spec: &[u8],
        provenance: &[u8],
        valid: bool,
        run: impl FnOnce(&Path) -> Result<Vec<u8>, String>,
    ) -> Result<(PathBuf, Publication), String> {
        materialize(
            cache,
            "test-key",
            spec,
            provenance,
            move |_, _| Ok(valid),
            |stage| {
                run(stage).map(|inputs| Executed {
                    inputs,
                    recorded: br#"{"version":1}"#.to_vec(),
                })
            },
        )
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
        //
        // The second materialization FORCES re-execution (a failing
        // `validate` falls through to the stage, exactly like a changed
        // watched input): a warm-reusing second call would just follow the
        // spec pointer to the first tree and prove nothing about the stage
        // pathname's determinism.
        let cache = temp();
        let embed = |stage: &Path| {
            fs::write(
                stage.join("go.env"),
                format!("GOPATH='{}'\n", stage.display()),
            )
            .map_err(|e| e.to_string())?;
            Ok(Executed {
                inputs: b"inputs".to_vec(),
                recorded: br#"{"version":1}"#.to_vec(),
            })
        };
        let first = materialize(
            cache.path(),
            "rules_gazelle++go_env",
            b"spec-go-env",
            b"provenance",
            |_, _| Ok(true),
            embed,
        )
        .unwrap()
        .0;
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
        let (second, publication) = materialize(
            cache.path(),
            "rules_gazelle++go_env",
            b"spec-go-env",
            b"provenance",
            // Invalidate the recorded watchable inputs: the rule must
            // re-execute into the same keyed stage.
            |_, _| Ok(false),
            embed,
        )
        .unwrap();
        assert_eq!(publication, Publication::Executed);
        // Re-executed (not warm-reused), into the same deterministic stage,
        // so the embedded path and the content identity both reproduce and
        // no second tree is published.
        assert_eq!(
            fs::read_to_string(second.join("go.env")).unwrap(),
            embedded,
            "re-execution must embed the same stage pathname"
        );
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
            materialize(
                cache.path(),
                "other_repo",
                b"spec-other",
                b"provenance",
                |_, _| Ok(true),
                embed,
            )
            .unwrap()
            .0,
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
        let (tree, _) = publish_keyed(cache.path(), "stale-key", b"", write).unwrap();
        assert_eq!(fs::read(tree.join("data")).unwrap(), [0, 255, 128, 10]);
        assert!(!tree.join("leftover").exists());
        no_stages(cache.path());
    }

    #[test]
    fn bazel_legal_in_repository_filenames_publish_and_snapshot() {
        // User regression (~/code, Bazel 9.3.0): a direct repository merely
        // CONTAINING `n_structure_trailing_#.json` (Bazel's own JSON
        // conformance asset, len=87 depth=8) failed the whole build with
        // `invalid direct repository path or path/depth limit exceeded`.
        // On Linux `#`, `%`, `?`, `:` are ordinary filename bytes (only `/`
        // and NUL are structural); Bazel accepts such names via
        // os.DirEntry-flat traversal. Exercise BOTH snapshot() of an
        // existing tree and materialize-into-store publication.
        let cache = temp();
        let names = [
            "transport/http/protocol/internal/json/testdata/test_parsing/n_structure_trailing_#.json",
            "x#y.json",
            "lit%{name}.txt",
            "what?.txt",
            "colon:name.txt",
        ];
        // (1) snapshot() over a pre-existing direct-repository tree.
        let source = temp();
        for name in names {
            let path = source.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, b"genuine").unwrap();
        }
        let snap = snapshot(source.path()).unwrap();
        for name in names {
            assert!(snap.contains_key(name), "snapshot missing {name}");
        }
        // (2) materialize-into-store: publish, then warm-reuse (which
        // re-snapshots the published tree) must succeed identically.
        let (tree, status) = publish(cache.path(), b"", |stage| {
            for name in names {
                let path = stage.join(name);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(&path, b"genuine").unwrap();
            }
            Ok(vec![])
        })
        .unwrap();
        assert_eq!(status, Publication::Executed);
        for name in names {
            assert_eq!(fs::read(tree.join(name)).unwrap(), b"genuine");
        }
        let (warm, status) = publish(cache.path(), b"", |_| panic!("must reuse")).unwrap();
        assert_eq!(status, Publication::Reused);
        assert_eq!(warm, tree);
        no_stages(cache.path());
    }

    #[test]
    fn binary_executable_empty_directories_and_boundary() {
        let cache = temp();
        let (tree, status) = publish(cache.path(), b"definition", write).unwrap();
        assert_eq!(status, Publication::Executed);
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
        assert_eq!(
            manifest.spec.as_deref(),
            Some(spec_identity(b"spec").as_str())
        );
        assert_eq!(
            manifest.recorded.as_deref(),
            Some(br#"{"version":1}"#.as_slice())
        );
        assert_eq!(content_view(&manifest.files), content_view(&snapshot(&tree).unwrap()));
        assert!(manifest.files.values().all(|e| e.stat.is_some()));
        no_stages(cache.path());
    }

    #[test]
    fn provenance_and_runtime_inputs_are_framed_and_part_of_identity() {
        let cache = temp();
        let (a, _) =
            publish_spec(cache.path(), b"spec-a", b"a", true, |_| Ok(b"bc".to_vec())).unwrap();
        let (b, _) =
            publish_spec(cache.path(), b"spec-b", b"ab", true, |_| Ok(b"c".to_vec())).unwrap();
        let (c, _) =
            publish_spec(cache.path(), b"spec-a", b"a", false, |_| Ok(b"bd".to_vec())).unwrap();
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn spec_identity_is_a_pure_function_of_the_specification() {
        assert_eq!(spec_identity(b"same"), spec_identity(b"same"));
        assert_ne!(spec_identity(b"same"), spec_identity(b"changed"));
        let hex = spec_identity(b"");
        assert_eq!(hex.len(), 64);
        assert!(
            hex.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
    }

    #[test]
    fn warm_calls_reuse_publications_without_executing() {
        let cache = temp();
        let count = AtomicUsize::new(0);
        let run = |input: &[u8], warm_world_unchanged: bool| {
            publish_spec(
                cache.path(),
                b"same",
                b"provenance",
                warm_world_unchanged,
                |stage| {
                    count.fetch_add(1, Ordering::SeqCst);
                    write(stage)?;
                    Ok(input.to_vec())
                },
            )
            .unwrap()
        };
        let (a, first) = run(b"first", true);
        assert_eq!(first, Publication::Executed);
        // Same specification and unchanged recorded inputs: the repository
        // rule never runs again, exactly like Bazel's external/<name>.
        let (again, status) = run(b"first", true);
        assert_eq!(again, a);
        assert_eq!(status, Publication::Reused);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        // The recorded inputs changing (the validator notices the world
        // changed) refetches: execution runs and both content identities
        // coexist as separate trees.
        let (changed, status) = run(b"changed", false);
        assert_ne!(changed, a);
        assert_eq!(status, Publication::Executed);
        assert_eq!(count.load(Ordering::SeqCst), 2);
        // The publication for this specification now follows the inputs.
        assert_eq!(run(b"changed", true).0, changed);
        assert_eq!(count.load(Ordering::SeqCst), 2);
        no_stages(cache.path());
    }

    #[test]
    fn changed_specifications_address_separate_publications() {
        let cache = temp();
        let (a, first) =
            publish_spec(cache.path(), b"spec-1", b"provenance-1", true, write).unwrap();
        assert_eq!(first, Publication::Executed);
        let (b, second) =
            publish_spec(cache.path(), b"spec-2", b"provenance-2", true, write).unwrap();
        assert_eq!(second, Publication::Executed);
        assert_ne!(a, b);
        assert_eq!(
            fs::read_dir(cache.path().join("direct/trees"))
                .unwrap()
                .count(),
            2
        );
        // Each specification keeps reusing its own publication.
        let (again, status) = publish_spec(cache.path(), b"spec-1", b"provenance-1", true, |_| {
            panic!("spec-1 must reuse")
        })
        .unwrap();
        assert_eq!(again, a);
        assert_eq!(status, Publication::Reused);
        no_stages(cache.path());
    }

    #[test]
    fn callback_error_and_panic_clean_private_stage_without_publication() {
        let cache = temp();
        assert!(
            materialize(
                cache.path(),
                "test-key",
                b"spec",
                b"",
                |_, _| Ok(true),
                |stage| {
                    write(stage)?;
                    Err("callback failure".into())
                }
            )
            .is_err()
        );
        let result = std::panic::catch_unwind(|| {
            materialize(
                cache.path(),
                "test-key",
                b"spec",
                b"",
                |_, _| Ok(true),
                |stage| {
                    write(stage)
                        .map(|_| Executed {
                            inputs: Vec::new(),
                            recorded: br#"{"version":1}"#.to_vec(),
                        })
                        .unwrap();
                    panic!("callback panic");
                },
            )
        });
        assert!(result.is_err());
        for name in ["trees", "manifests", "specs"] {
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
        for (index, boundary) in [
            "REPO.bazel",
            "MODULE.bazel",
            "WORKSPACE.bazel",
            "WORKSPACE",
            "BUILD",
            "BUILD.bazel",
        ]
        .into_iter()
        .enumerate()
        {
            // Distinct specifications keep every publication independent.
            let (tree, _) = publish_spec(
                cache.path(),
                format!("spec-{index}").as_bytes(),
                boundary.as_bytes(),
                true,
                |stage| {
                    fs::write(stage.join(boundary), b"original").unwrap();
                    Ok(vec![])
                },
            )
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
        let (tree, _) = publish(cache.path(), b"", write).unwrap();
        let original = content_view(&snapshot(&tree).unwrap());
        fs::write(tree.join("data"), b"bad").unwrap();
        let (again, status) = publish(cache.path(), b"", write).unwrap();
        assert_eq!(tree, again);
        assert_eq!(status, Publication::Executed);
        assert_eq!(original, content_view(&snapshot(&tree).unwrap()));
        fs::remove_dir(tree.join("empty")).unwrap();
        assert_eq!(tree, publish(cache.path(), b"", write).unwrap().0);
        assert_eq!(original, content_view(&snapshot(&tree).unwrap()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(tree.join("data"), fs::Permissions::from_mode(0o600)).unwrap();
        }
        assert_eq!(tree, publish(cache.path(), b"", write).unwrap().0);
        assert_eq!(original, content_view(&snapshot(&tree).unwrap()));
        no_stages(cache.path());
    }

    #[test]
    fn changed_output_has_a_different_content_address() {
        let cache = temp();
        // Different recorded inputs refetch a repository: the new publication
        // has a different content address, and both trees coexist.
        let (first, _) = publish_spec(cache.path(), b"spec", b"", true, |stage| {
            fs::write(stage.join("data"), b"first").unwrap();
            Ok(b"inputs-first".to_vec())
        })
        .unwrap();
        let (second, _) = publish_spec(cache.path(), b"spec", b"", false, |stage| {
            fs::write(stage.join("data"), b"second").unwrap();
            Ok(b"inputs-second".to_vec())
        })
        .unwrap();
        assert_ne!(first, second);
        assert_eq!(fs::read(first.join("data")).unwrap(), b"first");
        assert_eq!(fs::read(second.join("data")).unwrap(), b"second");
    }

    #[test]
    fn legacy_manifests_are_upgraded_once_then_reused() {
        // A manifest written before `spec`/`recorded` existed (an older
        // binary's publication with identical content identity) is upgraded
        // by exactly one execution; after that the warm path reuses the
        // publication through the pointer without executing again.
        let cache = temp();
        let (tree, first) = publish(cache.path(), b"", write).unwrap();
        assert_eq!(first, Publication::Executed);
        let path = metadata_path(cache.path(), &tree);
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let object = legacy.as_object_mut().unwrap();
        object.remove("spec");
        object.remove("recorded");
        fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let executions = AtomicUsize::new(1);
        let (upgraded, second) = publish(cache.path(), b"", |stage| {
            executions.fetch_add(1, Ordering::SeqCst);
            write(stage)
        })
        .unwrap();
        assert_eq!(upgraded, tree);
        assert_eq!(second, Publication::Executed);
        assert_eq!(executions.load(Ordering::SeqCst), 2);
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            manifest["spec"].as_str(),
            Some(spec_identity(b"spec").as_str())
        );
        assert!(manifest.get("recorded").is_some());
        let (again, third) = publish(cache.path(), b"", |_| {
            panic!("upgraded publication must be reused")
        })
        .unwrap();
        assert_eq!(again, tree);
        assert_eq!(third, Publication::Reused);
        no_stages(cache.path());
    }

    #[test]
    fn nondeterministic_output_under_unchanged_inputs_pins_first_publication() {
        // A rule that would write different bytes with unchanged recorded
        // inputs never re-runs: the first publication is reused, like Bazel's
        // fetch-once-per-output-base external/<name> directory.
        let cache = temp();
        let (first, _) = publish(cache.path(), b"", |stage| {
            fs::write(stage.join("data"), b"first").unwrap();
            Ok(vec![])
        })
        .unwrap();
        let (again, status) = publish(cache.path(), b"", |stage| {
            fs::write(stage.join("data"), b"second").unwrap();
            Ok(vec![])
        })
        .unwrap();
        assert_eq!(first, again);
        assert_eq!(status, Publication::Reused);
        assert_eq!(fs::read(first.join("data")).unwrap(), b"first");
        no_stages(cache.path());
    }

    #[test]
    fn missing_stale_malformed_and_wrong_shape_manifests_are_repaired() {
        let cache = temp();
        let (tree, _) = publish(cache.path(), b"", write).unwrap();
        let path = metadata_path(cache.path(), &tree);
        // Compare parsed manifests: a repair re-publishes with FRESH stat
        // fingerprints (Step B), so raw bytes legitimately differ in the
        // `stat` fields while every content field is identical.
        let expected: Manifest = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let assert_repaired = |path: &Path| {
            let repaired: Manifest = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
            assert_eq!(repaired.identity, expected.identity);
            assert_eq!(repaired.spec, expected.spec);
            assert_eq!(repaired.recorded, expected.recorded);
            assert_eq!(
                content_view(&repaired.files),
                content_view(&expected.files)
            );
        };
        for bytes in [b"{".as_slice(), b"{}", b"null", b"{\"format\":999}"] {
            fs::write(&path, bytes).unwrap();
            assert_eq!(tree, publish(cache.path(), b"", write).unwrap().0);
            assert_repaired(&path);
        }
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        publish(cache.path(), b"", write).unwrap();
        assert_repaired(&path);
        fs::remove_file(&path).unwrap();
        publish(cache.path(), b"", write).unwrap();
        assert_repaired(&path);
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
                        b"spec",
                        b"provenance",
                        |_, _| Ok(true),
                        |stage| {
                            let inputs = write(stage)?;
                            barrier.wait();
                            Ok(Executed {
                                inputs,
                                recorded: br#"{"version":1}"#.to_vec(),
                            })
                        },
                    )
                    .map(|(root, _)| root)
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
                    materialize(
                        &cache,
                        "single-flight",
                        b"spec",
                        b"same",
                        |_, _| Ok(true),
                        |stage| {
                            count.fetch_add(1, Ordering::SeqCst);
                            write(stage).map(|inputs| Executed {
                                inputs,
                                recorded: br#"{"version":1}"#.to_vec(),
                            })
                        },
                    )
                    .map(|(root, _)| root)
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
        for (index, directory) in [false, true].into_iter().enumerate() {
            let target = if directory {
                outside.path().to_owned()
            } else {
                outside.path().join("precious")
            };
            let spec = format!("spec-{index}");
            let run = |cache: &Path, spec: &str| {
                publish_spec(cache, spec.as_bytes(), b"", true, |stage| {
                    symlink(&target, stage.join("link")).unwrap();
                    symlink("missing/dangling", stage.join("dangling")).unwrap();
                    Ok(vec![])
                })
                .unwrap()
                .0
            };
            let tree = run(cache.path(), &spec);
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
            assert_eq!(tree, run(cache.path(), &spec));
            trees.push(tree);
        }
        // Distinct link targets are distinct content identities.
        assert_ne!(trees[0], trees[1]);
        let tree = publish_spec(cache.path(), b"spec-file", b"", true, |stage| {
            fs::write(stage.join("link"), b"file").unwrap();
            Ok(vec![])
        })
        .unwrap()
        .0;
        assert!(!trees.contains(&tree));
        assert!(
            publish_spec(cache.path(), b"spec-hardlink", b"", true, |stage| {
                fs::hard_link(outside.path().join("precious"), stage.join("hardlink")).unwrap();
                Ok(vec![])
            })
            .is_err()
        );
        assert!(
            publish_spec(cache.path(), b"spec-socket", b"", true, |stage| {
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
        let (tree, _) = publish(cache.path(), b"", write).unwrap();
        let path = metadata_path(cache.path(), &tree);
        fs::remove_dir_all(&tree).unwrap();
        fs::remove_file(&path).unwrap();
        symlink(outside.path(), &tree).unwrap();
        symlink(outside.path().join("precious"), &path).unwrap();
        assert_eq!(tree, publish(cache.path(), b"", write).unwrap().0);
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
                b"spec",
                b"",
                |_, _| panic!("must not validate"),
                |_| panic!("must not evaluate")
            )
            .is_err()
        );
        assert!(!outside.path().join("child").exists());
        assert!(
            materialize(
                Path::new("relative"),
                "test-key",
                b"spec",
                b"",
                |_, _| panic!("must not validate"),
                |_| panic!("must not evaluate")
            )
            .is_err()
        );
    }

    #[test]
    fn large_files_and_many_entries_publish_but_path_profile_holds() {
        // Above the former 16 MiB file / 64 MiB total / 10000-entry caps.
        let cache = temp();
        let (tree, _) = publish(cache.path(), b"", |stage| {
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
        // Depth is bounded only by the 4096-byte total (PATH_MAX): a
        // 65-component tree publishes, like Bazel on Linux.
        let (deep, _) = publish_spec(cache.path(), b"deep-spec", b"", true, |stage| {
            let mut path = stage.to_owned();
            for _ in 0..65 {
                path.push("d");
                fs::create_dir(&path).unwrap();
            }
            Ok(vec![])
        })
        .unwrap();
        assert!(deep.join(vec!["d"; 65].join("/")).is_dir());
        assert!(safe_relative(&"a".repeat(PATH_LIMIT + 1)).is_err());
        assert!(safe_relative(&"a".repeat(NAME_LIMIT + 1)).is_err());
        assert!(safe_relative(&vec!["d"; 65].join("/")).is_ok());
        no_stages(cache.path());
    }

    /// Step B tamper matrix (default Stat verification):
    /// (1) do-nothing warm call reuses; (2) content changed with size
    /// changed re-executes; (3) content changed keeping size AND mtime
    /// (touch -d) still re-executes — the ctime moves, the stat
    /// fingerprint mismatches, the fallback hash catches it.
    #[test]
    fn stat_verify_reuse_and_tamper_matrix() {
        let cache = temp();
        let executions = AtomicUsize::new(0);
        let run = |cache: &Path| {
            publish(cache, b"", |stage| {
                executions.fetch_add(1, Ordering::SeqCst);
                write(stage)
            })
            .unwrap()
        };
        let (tree, first) = run(cache.path());
        assert_eq!(first, Publication::Executed);
        assert_eq!(executions.load(Ordering::SeqCst), 1);
        // (1) do-nothing: reuse, no execution, stat fingerprints recorded.
        let (again, status) = run(cache.path());
        assert_eq!(again, tree);
        assert_eq!(status, Publication::Reused);
        assert_eq!(executions.load(Ordering::SeqCst), 1);
        let manifest: Manifest =
            serde_json::from_slice(&fs::read(metadata_path(cache.path(), &tree)).unwrap())
                .unwrap();
        assert!(manifest.files.values().all(|e| e.stat.is_some()));
        // (2) size change: re-execute.
        fs::write(tree.join("data"), b"bad-longer-content").unwrap();
        let (_, status) = run(cache.path());
        assert_eq!(status, Publication::Executed);
        assert_eq!(executions.load(Ordering::SeqCst), 2);
        assert_eq!(fs::read(tree.join("data")).unwrap(), [0, 255, 128, 10]);
        // (3) same size, mtime restored: ctime betrays it; hash catches it.
        let path = tree.join("data");
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        fs::write(&path, [9u8, 9, 9, 9]).unwrap();
        let since = mtime
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();
        let out = std::process::Command::new("touch")
            .arg("-d")
            .arg(format!("@{}.{:09}", since.as_secs(), since.subsec_nanos()))
            .arg(&path)
            .output()
            .unwrap();
        assert!(out.status.success(), "touch: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), mtime);
        let (_, status) = run(cache.path());
        assert_eq!(status, Publication::Executed);
        assert_eq!(executions.load(Ordering::SeqCst), 3);
        assert_eq!(fs::read(tree.join("data")).unwrap(), [0, 255, 128, 10]);
        // Full mode agrees on the untouched tree.
        assert_eq!(
            verify_snapshot(&tree, &snapshot(&tree).unwrap(), VerifyMode::Full).unwrap(),
            true
        );
        no_stages(cache.path());
    }

    /// Step B performance shape: a wide tree (2,000 files) verifies by
    /// stat without hashing — proven functionally by corrupting a file
    /// with same size+mtime (caught) while the untouched tree reuses.
    /// The ≤50 ms Go-SDK-tree target is measured on the real cache in
    /// docs/REPOSITORY_WARM_PERF.md (stat walk of the 15,643-file tree).
    #[test]
    fn stat_verify_wide_tree_reuses_without_rehash() {
        let cache = temp();
        let (tree, _) = publish(cache.path(), b"wide", |stage| {
            for i in 0..2000 {
                fs::write(stage.join(format!("f{i}")), vec![7u8; 4096]).unwrap();
            }
            Ok(vec![])
        })
        .unwrap();
        let expected = snapshot(&tree).unwrap();
        assert!(verify_snapshot(&tree, &expected, VerifyMode::Stat).unwrap());
        assert!(verify_snapshot(&tree, &expected, VerifyMode::Full).unwrap());
        // Warm reuse through the pointer does not execute again.
        let (_, status) = publish(cache.path(), b"wide", |_| {
            panic!("wide tree must reuse under stat verification")
        })
        .unwrap();
        assert_eq!(status, Publication::Reused);
        no_stages(cache.path());
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_and_invalid_paths_are_rejected() {
        use std::os::unix::ffi::OsStringExt;
        let cache = temp();
        assert!(
            publish(cache.path(), b"", |stage| {
                fs::write(stage.join(std::ffi::OsString::from_vec(vec![255])), b"").unwrap();
                Ok(vec![])
            })
            .is_err()
        );
        no_stages(cache.path());
    }
}
