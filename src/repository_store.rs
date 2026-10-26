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
/// The manifest schema this binary writes and trusts (see [`Manifest`]).
const MANIFEST_FORMAT: u32 = 1;
const PATH_LIMIT: usize = 4096;
const NAME_LIMIT: usize = 255;
/// Bumped (v1 -> v2) when repository rules moved from private stages into
/// their slots `<store>/external/<name>` (crate::repository_slots): a
/// publication an earlier binary made in a stage may embed that stage's path
/// (gazelle's go.env, a `-L../<stage>` link flag), so no pointer of the
/// earlier profile is ever reused; identical content keeps its identity.
const SPEC_PROFILE: &[u8] = b"direct-repository-spec-v2";

/// A publication's manifest.
///
/// Forward compatibility (review i20): readers TOLERATE unknown fields. A
/// newer binary's additive metadata (as `spec`, `recorded` and `Entry.stat`
/// once were) is ignored instead of making this binary discard a valid
/// publication and republish it — the churn every such addition used to
/// cause between versions. Integrity does not rest on rejecting unknown
/// fields but on checks that stay strict: the schema gate
/// ([`parse_manifest`]: `format` and `profile` exactly this binary's; a
/// change to what an existing field means, or a field that must not be
/// ignored, must bump `format`), the required fields and their types, the
/// identity the pointer names, the recorded inputs `validate` rechecks and
/// the tree verified against the known entry fields. Writers are
/// unchanged, and a manifest this binary did not write byte-for-byte is
/// never rewritten in place (`refresh_manifest`), so a newer binary's
/// fields are never silently stripped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Opaque recorded watchable inputs (caller JSON with a numeric
    /// `version`, [`recorded_version`]) rechecked by `validate`
    /// on warm calls before any reuse; never part of the identity digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recorded: Option<Vec<u8>>,
    /// The tree embeds its repository's own directory (`<store>/external/
    /// <name>`, crate::repository_slots): its slot stays a real, mutable
    /// directory and the tree is a copy. Written only when true; a manifest
    /// without it (every ordinary publication, and those of binaries before
    /// the field) describes an ordinary one. A function of the content (the
    /// identity covers it) for one store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    self_ref: Option<bool>,
    /// The other store trees the tree's bytes name (`direct/trees/<id>`,
    /// `trees/<id>`, ...: a GOROOT in `go.env`, absolute links into another
    /// tree), for a collector; informational, never compared.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    refs: Vec<String>,
}

impl Manifest {
    /// The slot class this publication's manifest records.
    fn slot_class(&self) -> crate::repository_slots::SlotClass {
        if self.self_ref == Some(true) {
            crate::repository_slots::SlotClass::SelfReferencing
        } else {
            crate::repository_slots::SlotClass::Ordinary
        }
    }
}

/// One tree entry of a [`Manifest`]. Unknown fields are tolerated: reuse
/// re-derives every known field (type, mode, size, digest, symlink target)
/// from the tree on disk, and identity/equality compare
/// [`content_view`]s of the known content fields, so an unrecognized
/// per-entry attribute can never widen what is trusted. The stat
/// fingerprint ([`StatFingerprint`], always tolerant) only narrows trust:
/// it must equal the current stat outside the racy window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// The manifest in `bytes`, if this binary may trust it: it parses (unknown
/// fields ignored, see [`Manifest`]) and passes the strict schema gate.
fn parse_manifest(bytes: &[u8]) -> Option<Manifest> {
    serde_json::from_slice::<Manifest>(bytes)
        .ok()
        .filter(|manifest| manifest.format == MANIFEST_FORMAT && manifest.profile == PROFILE)
}

/// The version of opaque recorded-probe bytes: every caller records a JSON
/// object with a numeric `version` (`repository_warm::RecordedInputs`,
/// version 1 since recorded probes exist). None when the bytes carry none
/// this binary can read — a format it does not know.
fn recorded_version(recorded: &[u8]) -> Option<u64> {
    #[derive(Deserialize)]
    struct Versioned {
        version: u64,
    }
    serde_json::from_slice::<Versioned>(recorded)
        .ok()
        .map(|versioned| versioned.version)
}

/// The workspace recorded probes name (`workspace`, a string: the
/// publication depends on where that workspace is, docs/WORKTREES.md), if
/// any.
fn recorded_workspace(recorded: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct Located {
        #[serde(default)]
        workspace: Option<String>,
    }
    serde_json::from_slice::<Located>(recorded)
        .ok()
        .and_then(|located| located.workspace)
}

/// Whether an execution that recorded `new` probes and kept a publication's
/// identity republishes it over the persisted manifest's `existing` probes
/// (review i20): to add missing probes (a manifest older than the field),
/// or to replace different probes of the same or an OLDER version. Never
/// over a newer version, nor over probes whose version this binary cannot
/// read: it cannot validate them (so it executes), but replacing them
/// would strip a newer binary's manifest, which would then re-execute and
/// republish in turn — the i18.1↔i19 ping-pong at every probe-format bump.
/// One-directional like the forward-compatible manifest readers.
fn upgrades_recorded(existing: Option<&[u8]>, new: &[u8]) -> bool {
    let Some(existing) = existing else {
        return true;
    };
    existing != new
        && matches!(
            (recorded_version(existing), recorded_version(new)),
            (Some(old), Some(new)) if old <= new
        )
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
                crate::diagnostic_switch(std::env::var_os("BAZEL_RS_TIMING")),
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
        /// Files actually hashed during warm verification (review R3):
        /// acceptance requires 0 after a run whose repos were staged by a
        /// build (fingerprints recorded settled, outside the racy window).
        pub files_hashed: AtomicU64,
        /// Regular files hashed for recorded-input file states (watched or
        /// read files, at execution and warm validation), and their bytes.
        pub recorded_hashed: AtomicU64,
        pub recorded_hashed_bytes: AtomicU64,
        /// Recorded-input file states answered by the in-process digest
        /// memo instead (review i20: `bin/gazelle` and `bin/fetch_repo` per
        /// go_repository), and the bytes they did not re-hash.
        pub recorded_memo_hits: AtomicU64,
        pub recorded_memo_hit_bytes: AtomicU64,
        pub execute: Acc,
        pub publish: Acc,
        pub spec_serialize: Acc,
        pub prepare_group: Acc,
        pub reused: AtomicU64,
        pub executed: AtomicU64,
        pub roots: std::sync::Mutex<Vec<(String, f64, &'static str)>>,
        /// Executed repositories whose rule is `local = True`: re-run by
        /// every invocation by design (RepositoryFetchFunction.
        /// shouldUseCachedRepoContents, :502-511), never reused.
        pub local: std::sync::Mutex<Vec<String>>,
    }
    pub static PHASES: Phases = Phases {
        manifest_read: Acc {
            calls: AtomicU64::new(0),
            nanos: AtomicU64::new(0),
        },
        recorded_check: Acc {
            calls: AtomicU64::new(0),
            nanos: AtomicU64::new(0),
        },
        snapshot_check: Acc {
            calls: AtomicU64::new(0),
            nanos: AtomicU64::new(0),
        },
        snapshot_files: AtomicU64::new(0),
        snapshot_bytes: AtomicU64::new(0),
        files_hashed: AtomicU64::new(0),
        recorded_hashed: AtomicU64::new(0),
        recorded_hashed_bytes: AtomicU64::new(0),
        recorded_memo_hits: AtomicU64::new(0),
        recorded_memo_hit_bytes: AtomicU64::new(0),
        execute: Acc {
            calls: AtomicU64::new(0),
            nanos: AtomicU64::new(0),
        },
        publish: Acc {
            calls: AtomicU64::new(0),
            nanos: AtomicU64::new(0),
        },
        spec_serialize: Acc {
            calls: AtomicU64::new(0),
            nanos: AtomicU64::new(0),
        },
        prepare_group: Acc {
            calls: AtomicU64::new(0),
            nanos: AtomicU64::new(0),
        },
        reused: AtomicU64::new(0),
        executed: AtomicU64::new(0),
        roots: std::sync::Mutex::new(Vec::new()),
        local: std::sync::Mutex::new(Vec::new()),
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

    /// One recorded-input file digest of `bytes`: hashed, or a memo hit.
    pub fn count_recorded_digest(memo_hit: bool, bytes: u64) {
        if enabled() {
            let (files, total) = if memo_hit {
                (&PHASES.recorded_memo_hits, &PHASES.recorded_memo_hit_bytes)
            } else {
                (&PHASES.recorded_hashed, &PHASES.recorded_hashed_bytes)
            };
            files.fetch_add(1, Ordering::Relaxed);
            total.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    pub fn record_root(stage_key: &str, secs: f64, outcome: &'static str) {
        if enabled()
            && let Ok(mut roots) = PHASES.roots.lock()
        {
            roots.push((stage_key.to_owned(), secs, outcome));
        }
    }

    /// An execution of the `local = True` rule of repository `stage_key`.
    pub fn record_local_execution(stage_key: &str) {
        if enabled()
            && let Ok(mut local) = PHASES.local.lock()
        {
            local.push(stage_key.to_owned());
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
            "PERF-REPO files_hashed_during_verify={}",
            p.files_hashed.load(Ordering::Relaxed),
        );
        eprintln!(
            "PERF-REPO recorded-input file digests: hashed={} ({} bytes) memo_hits={} ({} bytes not re-hashed)",
            p.recorded_hashed.load(Ordering::Relaxed),
            p.recorded_hashed_bytes.load(Ordering::Relaxed),
            p.recorded_memo_hits.load(Ordering::Relaxed),
            p.recorded_memo_hit_bytes.load(Ordering::Relaxed),
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
        // Every execution, not only the slowest: a warm invocation must
        // re-run nothing but `local = True` rules.
        let local = p.local.lock().map(|l| l.clone()).unwrap_or_default();
        let mut executed: Vec<&str> = roots
            .iter()
            .filter(|(_, _, outcome)| *outcome == "executed")
            .map(|(key, _, _)| key.as_str())
            .collect();
        executed.sort_unstable();
        let (local_executed, other): (Vec<&str>, Vec<&str>) = executed
            .iter()
            .partition(|key| local.iter().any(|name| name == *key));
        eprintln!(
            "PERF-REPO executed roots={} local={} (local = True: re-run by every invocation) other={}{}{}",
            executed.len(),
            local_executed.len(),
            other.len(),
            if other.is_empty() { "" } else { ": " },
            other.join(" "),
        );
        if !local_executed.is_empty() {
            eprintln!(
                "PERF-REPO executed local roots: {}",
                local_executed.join(" ")
            );
        }
    }
}

/// What one execution observed, for the identity digest and the manifest.
pub struct Executed {
    /// Exact serialized runtime input provenance, after validating the result.
    /// These bytes (never the incidental repository directory pathname)
    /// enter the identity.
    pub inputs: Vec<u8>,
    /// Opaque recorded watchable-input probes (caller JSON with a numeric
    /// `version`, see `recorded_version`) persisted in the manifest and
    /// rechecked by `validate` on later warm calls.
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

/// Bumped with [`SPEC_PROFILE`] (v1 -> v2); the streaming twins in
/// crate::repository_warm frame the same bytes.
pub(crate) const SPEC_DIGEST_PROFILE: &[u8] = b"direct-repository-spec-digest-v2";

/// Per-source digests of one loaded `.bzl` closure, computed ONCE per
/// prepared group per invocation (review R1): children of a group share
/// the same closure, so hashing it per repository re-hashed tens of MB
/// per sibling (64 GB per warm run). Rendering is byte-identical to the
/// per-call form it replaces, so existing specification pointers hit.
pub fn source_digest_entries(
    sources: &BTreeMap<String, crate::loading::SourceProvenance>,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let mut entries = serde_json::Map::new();
    for (label, source) in sources {
        let content = format!("{:x}", Sha256::digest(&source.bytes));
        let mapping = serde_json::to_string(&source.mapping)
            .map_err(|e| format!("cannot serialize repository mapping for {label}: {e}"))?;
        entries.insert(
            label.clone(),
            serde_json::json!({"sha256": content, "mapping": mapping}),
        );
    }
    Ok(entries)
}

/// Streaming twin of [`source_digest_entries`]: serialize each digest
/// entry value once, in label order, without building a `Value` tree.
/// Byte-identical to the `sources` object [`source_digest_entries`]
/// produces (JSON object keys sorted by `BTreeMap` iteration; serde_json
/// escapes strings deterministically), so existing pointers still hit.
pub fn source_digest_entries_bytes(
    sources: &BTreeMap<String, crate::loading::SourceProvenance>,
) -> Result<Vec<u8>, String> {
    // Cheap pre-pass: digests + mapping strings only (no source copying).
    let mut rendered: Vec<(&str, String, String)> = Vec::with_capacity(sources.len());
    for (label, source) in sources {
        let content = format!("{:x}", Sha256::digest(&source.bytes));
        let mapping = serde_json::to_string(&source.mapping)
            .map_err(|e| format!("cannot serialize repository mapping for {label}: {e}"))?;
        rendered.push((label.as_str(), content, mapping));
    }
    let mut out = Vec::with_capacity(2 + rendered.len() * 96);
    out.push(b'{');
    for (index, (label, content, mapping)) in rendered.iter().enumerate() {
        if index > 0 {
            out.push(b',');
        }
        // {"<label>":{"mapping":"<mapping-json-string>","sha256":"<hex>"}}
        // Key order inside the entry matches serde_json::json! with
        // preserve_order off? No: Map preserves insertion order, and the
        // Value form inserts "sha256" then "mapping" via json! — which
        // serde_json sorts? json! builds a Map; with preserve_order the
        // insertion order (sha256, mapping) is kept. Reproduce exactly.
        out.extend_from_slice(
            serde_json::to_string(label)
                .map_err(|e| format!("cannot serialize repository source label: {e}"))?
                .as_bytes(),
        );
        out.extend_from_slice(b":{\"sha256\":");
        out.extend_from_slice(
            serde_json::to_string(content)
                .map_err(|e| format!("cannot serialize repository source digest: {e}"))?
                .as_bytes(),
        );
        out.extend_from_slice(b",\"mapping\":");
        out.extend_from_slice(
            serde_json::to_string(mapping)
                .map_err(|e| format!("cannot serialize repository mapping for {label}: {e}"))?
                .as_bytes(),
        );
        out.push(b'}');
    }
    out.push(b'}');
    Ok(out)
}

/// The specification digest of an already-digested provenance: framed
/// SHA-256 over the group's source-digest entries plus the remaining
/// (per-repository) provenance fields. Serialization errors propagate
/// (review R1 item 4): defaulting them would key every repository to
/// the same pointer.
pub fn spec_digest_identity(
    provenance_fields: &serde_json::Value,
    source_entries: &serde_json::Map<String, serde_json::Value>,
) -> Result<String, String> {
    let mut digest = Sha256::new();
    digest.update(SPEC_DIGEST_PROFILE);
    digest.update([0]);
    // Frames the provenance fields exactly as the JSON object would
    // serialize with `sources` replaced by the group's digest entries.
    // In this test-only form `provenance_fields` carries no source bytes
    // (callers pass the digest-form object); production frames per-repo
    // bytes via SeededDigestWriter without materializing any Value.
    let mut framed = provenance_fields.clone();
    if let Some(object) = framed.as_object_mut() {
        object.insert(
            "sources".into(),
            serde_json::Value::Object(source_entries.clone()),
        );
    }
    let bytes = serde_json::to_vec(&framed)
        .map_err(|e| format!("cannot serialize repository specification: {e}"))?;
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(&bytes);
    Ok(format!("{:x}", digest.finalize()))
}

/// SHA-256 sink pre-seeded with `profile || 0 || be(len)` — the
/// [`spec_digest_identity`] framing — for streaming provenance
/// serialization. The length must be known first (a counting pass);
/// both passes are O(per-repo fields), never O(closure).
pub struct SeededDigestWriter {
    digest: Sha256,
}

impl SeededDigestWriter {
    pub fn new(profile: &[u8], framed_len: u64) -> Self {
        let mut digest = Sha256::new();
        digest.update(profile);
        digest.update([0]);
        digest.update(framed_len.to_be_bytes());
        Self { digest }
    }
    pub fn finish_hex(self) -> String {
        format!("{:x}", self.digest.finalize())
    }
}

impl Write for SeededDigestWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.digest.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Counting-only sink (first pass of the length-framed digest).
pub struct CountWriter(pub u64);

impl Write for CountWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn framed(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

/// Execute in the repository's directory, then verify and publish its
/// complete tree; or, when the specification already owns a valid
/// publication whose recorded watchable inputs are unchanged, reuse it
/// without executing. `execute` returns exact serialized runtime input
/// provenance plus recorded probes after validating its result.
/// `provenance` identifies the definition, attributes and mappings; it is
/// kept byte-for-byte in the identity digest so existing content identities
/// are unchanged. The directory is the slot `<cache>/external/<stage_key>`
/// (`stage_key` is the repository's canonical name; crate::repository_slots)
/// — Bazel's `<output_base>/external/<name>` — so repository content that
/// embeds its own working directory (`ctx.path(".")`, as gazelle's `go_env`
/// does, or a path derived from it, as gazelle's `${SRCDIR}` is) addresses a
/// path that is identical in every process and invocation, keeping the tree
/// identity stable. Concurrent evaluations of the same key serialize on the
/// slot's writer lock in the cache, like module extension working
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
    V: Fn(&Path, &[u8]) -> Result<bool, String>,
{
    materialize_keyed(
        cache,
        stage_key,
        &spec_identity(spec),
        provenance,
        validate,
        execute,
    )
}

/// Persist refreshed stat fingerprints into the manifest of publication
/// `identity` (review R3) — the only write of the warm probe, safe against
/// concurrent probes (worker threads, other processes) and publishers
/// (review i20):
/// - under the publication's lock `locks/<identity>`, which a publisher
///   holds while it replaces the manifest; the lock is only TRIED, never
///   awaited (no worker blocks; a busy lock means a publisher is writing
///   the manifest, with fingerprints of its own) and never created;
/// - compare-and-swap: the manifest is rewritten only if it still holds
///   exactly the `verified` bytes the probe checked the tree against, so
///   a newer publication or another probe's refresh wins, and repeating a
///   refresh changes nothing (idempotent);
/// - only a manifest this binary wrote byte-for-byte is rewritten (a newer
///   binary's fields are never stripped), through a temp file renamed over
///   it: readers see the old or the new manifest, never a partial one.
///
/// Best-effort by contract: a skip or failure only costs re-hashing on a
/// later run, never correctness.
fn refresh_manifest(
    direct: &Path,
    identity: &str,
    verified: &[u8],
    refreshed: &BTreeMap<String, StatFingerprint>,
) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.read(true);
    no_follow(&mut options);
    let Ok(lock) = options.open(direct.join("locks").join(identity)) else {
        return Ok(()); // no publication lock to coordinate with
    };
    regular_metadata(&lock.metadata().map_err(|e| e.to_string())?)?;
    if FileExt::try_lock_exclusive(&lock).is_err() {
        return Ok(()); // a publisher or another refresh is writing it
    }
    // The File releases the lock on every exit.
    let manifests = direct.join("manifests");
    let manifest_path = manifests.join(format!("{identity}.json"));
    if read_stable(&manifest_path).ok().as_deref() != Some(verified) {
        return Ok(()); // replaced since the probe read it: theirs wins
    }
    let Some(mut current) = parse_manifest(verified) else {
        return Ok(());
    };
    // Rewrite only what this binary wrote: re-serializing a newer binary's
    // manifest would drop the fields this one does not know.
    if serde_json::to_vec(&current).map_err(|e| e.to_string())? != verified {
        return Ok(());
    }
    let mut changed = false;
    for (name, fp) in refreshed {
        if let Some(entry) = current.files.get_mut(name)
            && entry.stat != Some(*fp)
        {
            entry.stat = Some(*fp);
            changed = true;
        }
    }
    if !changed {
        return Ok(());
    }
    let encoded = serde_json::to_vec(&current).map_err(|e| e.to_string())?;
    let mut stage = tempfile::NamedTempFile::new_in(&manifests)
        .map_err(|e| format!("cannot stage manifest refresh: {e}"))?;
    stage
        .write_all(&encoded)
        .and_then(|()| stage.as_file().sync_all())
        .map_err(|e| e.to_string())?;
    stage
        .persist(&manifest_path)
        .map_err(|e| format!("cannot publish manifest refresh: {e}"))?;
    Ok(())
}

/// Reuse-only probe (Step C): if the specification pointer names a valid
/// publication whose recorded inputs validate and whose tree verifies,
/// return its root WITHOUT executing and without taking any lock. Reads
/// only, except two best-effort writes: a fallback hash match may refresh
/// stat fingerprints into the manifest ([`refresh_manifest`]: try-locked,
/// compare-and-swap, atomic rename, idempotent), and the slot of repository
/// `stage_key` is bound to the reused tree when it is ordinary
/// (crate::repository_slots::bind: a link renamed into place, never over a
/// real directory) — safe on worker threads and across processes.
/// `Ok(None)` = no reusable publication (caller decides: sentinel under the
/// guard, execute else).
pub fn try_reuse<V>(
    cache: &Path,
    stage_key: &str,
    spec_key: &str,
    validate: V,
) -> Result<Option<PathBuf>, String>
where
    V: Fn(&Path, &[u8]) -> Result<bool, String>,
{
    try_reuse_in(cache, stage_key, spec_key, None, validate)
}

/// The name of the specification pointer of `spec_key` for the publication
/// of `workspace` (its canonical root as Latin-1 text, the `workspace` of
/// recorded probes, crate::repository_warm), when that publication depends
/// on where the workspace is (docs/WORKTREES.md): every worktree sharing a
/// cache keeps its own, so switching between them neither executes the
/// rule again nor changes a pointer another worktree's analysis snapshot
/// read. The pointer of the specification itself names only publications
/// that depend on no workspace.
pub fn workspace_pointer(spec_key: &str, workspace: &str) -> String {
    let digest = Sha256::digest(workspace.as_bytes());
    let hash: String = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{spec_key}.ws-{hash}")
}

/// [`try_reuse`] in `workspace` (see [`workspace_pointer`]; None: no
/// workspace pointer is looked at): the specification's own pointer first,
/// then, when it names nothing reusable, the workspace's pointer. Only the
/// pointer followed is read; the specification pointer's absence is
/// recorded for the analysis snapshot when the workspace's pointer is then
/// followed (a later shared publication must be seen).
pub fn try_reuse_in<V>(
    cache: &Path,
    stage_key: &str,
    spec_key: &str,
    workspace: Option<&str>,
    validate: V,
) -> Result<Option<PathBuf>, String>
where
    V: Fn(&Path, &[u8]) -> Result<bool, String>,
{
    let direct = cache.join("direct");
    let shared = direct.join("specs").join(spec_key);
    let found = reuse_through(cache, &shared, stage_key, spec_key, &validate)?;
    match (found, workspace) {
        (Reuse::Found(root), _) => Ok(Some(root)),
        (_, None) => Ok(None),
        (reuse, Some(workspace)) => {
            if reuse == Reuse::NoPointer {
                let _ = crate::observed_fs::symlink_metadata(&shared);
            }
            let own = direct
                .join("specs")
                .join(workspace_pointer(spec_key, workspace));
            Ok(
                match reuse_through(cache, &own, stage_key, spec_key, &validate)? {
                    Reuse::Found(root) => Some(root),
                    Reuse::NoPointer | Reuse::Invalid => None,
                },
            )
        }
    }
}

/// What following one specification pointer found ([`reuse_through`]).
#[derive(Debug, PartialEq, Eq)]
enum Reuse {
    /// The publication's root: reusable.
    Found(PathBuf),
    /// No pointer at that path.
    NoPointer,
    /// A pointer whose publication is not reusable here.
    Invalid,
}

/// Follow the specification pointer at `pointer` (of `spec_key`) to a
/// reusable publication of repository `stage_key` ([`try_reuse_in`]).
fn reuse_through(
    cache: &Path,
    pointer: &Path,
    stage_key: &str,
    spec_key: &str,
    validate: Validate<'_>,
) -> Result<Reuse, String> {
    let direct = &cache.join("direct");
    let Some(identity) = read_pointer(pointer)? else {
        return Ok(Reuse::NoPointer);
    };
    let destination = direct.join("trees").join(&identity);
    let manifest_path = direct.join("manifests").join(format!("{identity}.json"));
    let (bytes, existing) = {
        let _m = timing::mark(&timing::PHASES.manifest_read);
        // A missing manifest under a valid pointer may be a republication
        // between its two renames: looked for again (bounded).
        let Some(bytes) = read_published(&manifest_path, true).ok() else {
            return Ok(Reuse::Invalid);
        };
        let Some(existing) = parse_manifest(&bytes) else {
            return Ok(Reuse::Invalid);
        };
        (bytes, existing)
    };
    if existing.identity != identity || existing.recorded.is_none() {
        return Ok(Reuse::Invalid);
    }
    let recorded_ok = {
        let _m = timing::mark(&timing::PHASES.recorded_check);
        validate(
            &destination,
            existing.recorded.as_deref().unwrap_or_default(),
        )
        .unwrap_or(false)
    };
    if !recorded_ok {
        return Ok(Reuse::Invalid);
    }
    let verified = {
        let _m = timing::mark(&timing::PHASES.snapshot_check);
        timing::count_snapshot(
            existing.files.len() as u64,
            existing.files.values().map(|e| e.size).sum::<u64>(),
        );
        match verify_snapshot_detailed(&destination, &existing.files, verify_mode()) {
            Ok(outcome) => {
                if outcome.matched && !outcome.refreshed.is_empty() {
                    // Best-effort refresh (review R3): failure only costs
                    // re-hashing next time, never correctness.
                    let _ = refresh_manifest(direct, &identity, &bytes, &outcome.refreshed);
                }
                outcome.matched
            }
            Err(_) => false,
        }
    };
    if !verified {
        return Ok(Reuse::Invalid);
    }
    // REPO + INTEGRITY records of the analysis snapshot (after the
    // fingerprint refresh above, so the manifest's key is the final one).
    crate::observed_fs::repository_tree("direct", spec_key, &destination, Some(&manifest_path));
    settle_slot(cache, stage_key, &identity, existing.slot_class());
    timing::PHASES
        .reused
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(Reuse::Found(destination))
}

/// The publication and slot class of repository `canonical` whose root is
/// `root`: what this process materialized
/// (crate::repository_slots::noted), else — a root it did not materialize,
/// as a replay's pinned one — what the manifest of the direct tree `root`
/// records. `None` for a root that is no direct publication (registry,
/// Git and local roots are never self-referencing).
pub fn slot_class(
    store: &Path,
    canonical: &str,
    root: &Path,
) -> Option<(String, crate::repository_slots::SlotClass)> {
    if let Some(noted) =
        crate::repository_slots::noted(&crate::repository_slots::slot_path(store, canonical))
    {
        return Some(noted);
    }
    let tree =
        store_tree_of(store, root).filter(|tree| tree.kind == "direct" && tree.root == root)?;
    let manifest = parse_manifest(&read_stable(&tree.manifest).ok()?)?;
    if manifest.identity != tree.id {
        return None;
    }
    let class = manifest.slot_class();
    crate::repository_slots::note(store, canonical, &tree.id, class);
    Some((tree.id, class))
}

/// After a reuse: an ordinary repository's slot is bound to its tree (best
/// effort: a failure leaves the slot as it was, which only relative paths of
/// other rules' executions can observe); a self-referencing one's mutable
/// slot is left alone (its readers convert a missing one,
/// crate::repository_ctx). Either way this process notes what it uses.
fn settle_slot(
    cache: &Path,
    stage_key: &str,
    identity: &str,
    class: crate::repository_slots::SlotClass,
) {
    if class == crate::repository_slots::SlotClass::Ordinary {
        let _ = crate::repository_slots::bind(cache, stage_key, identity);
    }
    crate::repository_slots::note(cache, stage_key, identity, class);
}

/// [`materialize`] with the caller-computed specification pointer identity:
/// digest-form callers pass a streamed specification digest and skip
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
    V: Fn(&Path, &[u8]) -> Result<bool, String>,
{
    materialize_lazy(
        cache,
        stage_key,
        spec_key,
        || Ok(provenance.to_vec()),
        &validate,
        execute,
    )
}

/// [`materialize_keyed`] with lazily serialized provenance (review R1):
/// `provenance` runs ONLY on the execute path (after the warm probe
/// misses), so a warm reuse never serializes the closure-bearing bytes.
pub fn materialize_lazy<F, V, P>(
    cache: &Path,
    stage_key: &str,
    spec_key: &str,
    provenance: P,
    validate: V,
    execute: F,
) -> Result<(PathBuf, Publication), String>
where
    F: FnOnce(&Path) -> Result<Executed, String>,
    V: Fn(&Path, &[u8]) -> Result<bool, String>,
    P: FnOnce() -> Result<Vec<u8>, String>,
{
    materialize_lazy_in(
        cache, stage_key, spec_key, None, provenance, validate, execute,
    )
}

/// [`materialize_lazy`] in `workspace`: the warm probes also follow the
/// workspace's own pointer of a workspace-dependent publication
/// ([`try_reuse_in`], [`workspace_pointer`]).
#[allow(clippy::too_many_arguments)]
pub fn materialize_lazy_in<F, V, P>(
    cache: &Path,
    stage_key: &str,
    spec_key: &str,
    workspace: Option<&str>,
    provenance: P,
    validate: V,
    execute: F,
) -> Result<(PathBuf, Publication), String>
where
    F: FnOnce(&Path) -> Result<Executed, String>,
    V: Fn(&Path, &[u8]) -> Result<bool, String>,
    P: FnOnce() -> Result<Vec<u8>, String>,
{
    materialize_inner(
        cache,
        stage_key,
        spec_key,
        provenance,
        Probe::Warm {
            validate: &validate,
            workspace,
        },
        execute,
    )
}

/// A local repository rule's execution (PLAN-L2 update 1 §(a)): the digests
/// of the two runtime parts of its identity — the exact serialized runtime
/// inputs and the encoded tree inventory — and the identity they produced.
/// The rest of the identity (the provenance) is fixed by what the analysis
/// snapshot validates, so a replay reproducing both digests
/// ([`replay_local`]) reproduces the identity and the root path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalExecution {
    pub inputs_digest: [u8; 32],
    pub snapshot_digest: [u8; 32],
    pub identity: String,
}

/// [`materialize_lazy`] for a local repository rule (`local = True`): it
/// never reuses a publication across invocations (Bazel's
/// RepositoryFetchFunction.shouldUseCachedRepoContents keeps local
/// repositories refetching; every native invocation is a fresh server), so
/// no warm probe runs — the specification pointer is neither read nor
/// recorded, and its unchanged bytes are not rewritten — and the execution
/// is no taint of the analysis snapshot: the caller records it for replay
/// (crate::snapshot::replay) with the returned digests.
pub fn materialize_local<F, P>(
    cache: &Path,
    stage_key: &str,
    spec_key: &str,
    provenance: P,
    execute: F,
) -> Result<(PathBuf, LocalExecution), String>
where
    F: FnOnce(&Path) -> Result<Executed, String>,
    P: FnOnce() -> Result<Vec<u8>, String>,
{
    let mut execution = None;
    let (root, _) = materialize_inner(
        cache,
        stage_key,
        spec_key,
        provenance,
        Probe::Local(&mut execution),
        execute,
    )?;
    let execution =
        execution.ok_or("internal error: a local repository execution without digests")?;
    Ok((root, execution))
}

/// What replaying a local repository rule reproduced: the two digests a
/// [`LocalExecution`] records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replayed {
    pub inputs_digest: [u8; 32],
    pub snapshot_digest: [u8; 32],
}

/// Replay a local repository rule (crate::snapshot::replay, PLAN-L2 update
/// 1 §(a)): `execute` runs in the very directory, under the same writer
/// lock, as [`materialize_local`] uses for `stage_key` — its slot
/// `<cache>/external/<stage_key>`: an execution may observe its working
/// directory (`java -XshowSettings:properties` prints `user.dir`) — and the
/// digests of its identity inputs and tree are taken exactly as a full run
/// would. Nothing is published, no pointer or manifest is touched. The slot
/// is restored for the `recorded` publication afterwards: bound to its tree
/// (an ordinary repository), or — a self-referencing one whose replay
/// reproduced the recorded tree — kept as its mutable directory; else
/// removed.
pub fn replay_local<F>(
    cache: &Path,
    stage_key: &str,
    recorded: &LocalExecution,
    execute: F,
) -> Result<Replayed, String>
where
    F: FnOnce(&Path) -> Result<Executed, String>,
{
    ensure_directory(cache)?;
    let direct = cache.join("direct");
    let locks = direct.join("locks");
    for path in [&direct, &locks] {
        ensure_directory(path)?;
    }
    let writer = crate::repository_slots::SlotWriter::acquire(cache, stage_key)?;
    writer.unmark()?;
    let slot = writer.fresh()?;
    let scratch = crate::repository_slots::SlotScratch::new(&writer);
    let executed = execute(&slot)?;
    let mut embedded =
        crate::repository_slots::EmbeddedPaths::new(&canonical_store(cache), stage_key);
    let (_, encoded_snapshot) = stage_content(&slot, Some(&mut embedded))?;
    let embedded = embedded.finish();
    let replayed = Replayed {
        inputs_digest: Sha256::digest(&executed.inputs).into(),
        snapshot_digest: Sha256::digest(&encoded_snapshot).into(),
    };
    let tree = PublishedTree {
        kind: "direct",
        id: recorded.identity.clone(),
        root: direct.join("trees").join(&recorded.identity),
        manifest: direct
            .join("manifests")
            .join(format!("{}.json", recorded.identity)),
    };
    if is_published(&tree) {
        if !embedded.self_referencing {
            writer.bind(&recorded.identity)?;
            scratch.keep();
        } else if replayed.snapshot_digest == recorded.snapshot_digest {
            writer.mark(&recorded.identity)?;
            scratch.keep();
        }
    }
    Ok(replayed)
}

/// The store root as a repository rule sees it (its canonical path: the
/// slot a rule runs in is canonicalized, crate::repository_ctx), for the
/// self-reference scan.
fn canonical_store(cache: &Path) -> PathBuf {
    fs::canonicalize(cache).unwrap_or_else(|_| cache.to_path_buf())
}

/// A private directory of a repository store (a quarantine; also the Git
/// and registry stores' stages) that is deleted when dropped,
/// like the [`tempfile::TempDir`] that created it, but with
/// [`crate::repository_ctx::remove_tree`]: a repository rule may have made
/// directories read-only (`chmod -R ugo-w`, as rules_python does for its
/// interpreter's `lib/`), and `TempDir`'s `remove_dir_all` cannot empty
/// those. Symlinks are never followed. A removal failure only leaves
/// garbage behind (cleared by [`quarantine_lock`] or the next replacement)
/// and a warning; it never fails the command.
pub(crate) struct Scratch(PathBuf);

impl Scratch {
    pub(crate) fn new(directory: tempfile::TempDir) -> Self {
        Self(directory.keep())
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // Nothing to do once renamed into place (NotFound is success).
        if let Err(error) = crate::repository_ctx::remove_tree(&self.0) {
            eprintln!(
                "bazel-rs: warning: cannot remove {} ({error}); left for a later cleanup",
                self.0.display()
            );
        }
    }
}

/// The shared quarantine lock, held while this publisher's `.trash-*`
/// exists. Before taking it, a publisher that can take it EXCLUSIVELY — no
/// publisher of any process is between creating and removing its own
/// quarantine — first removes every `.trash-*` left in `trees` by an
/// earlier publication: one killed mid-publication, or a binary before
/// [`Scratch`], whose failed removal of a read-only tree failed the command
/// and left the quarantine behind. At most one such sweep per process and
/// store ([`swept_stores`]): leftovers only come from earlier processes
/// (this one removes its own quarantines, or warns), while sweeping on every
/// publication listed `trees` once per published tree — quadratic over a
/// cold fetch of hundreds of repositories — and briefly serialized
/// concurrent publishers behind the exclusive lock each time. A sweep that
/// could not take the lock is tried again by the next publication.
fn quarantine_lock(locks: &Path, trees: &Path) -> Result<File, String> {
    let path = locks.join("quarantine");
    if let Some(metadata) = optional_metadata(&path)? {
        regular_metadata(&metadata)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    no_follow(&mut options);
    let lock = options.open(&path).map_err(|e| e.to_string())?;
    regular_metadata(&lock.metadata().map_err(|e| e.to_string())?)?;
    {
        let mut swept = swept_stores().lock().unwrap_or_else(|e| e.into_inner());
        if !swept.contains(trees) && FileExt::try_lock_exclusive(&lock).is_ok() {
            if let Ok(entries) = fs::read_dir(trees) {
                for entry in entries.flatten() {
                    if entry.file_name().as_encoded_bytes().starts_with(b".trash-") {
                        let _ = crate::repository_ctx::remove_tree(&entry.path());
                    }
                }
            }
            swept.insert(trees.to_path_buf());
        }
    }
    // Converts an exclusive lock taken above.
    FileExt::lock_shared(&lock)
        .map_err(|e| format!("cannot lock direct repository quarantine: {e}"))?;
    Ok(lock)
}

/// The tree directories whose leftover quarantines this process has swept
/// ([`quarantine_lock`]).
fn swept_stores() -> &'static std::sync::Mutex<std::collections::HashSet<PathBuf>> {
    static SWEPT: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> =
        std::sync::OnceLock::new();
    SWEPT.get_or_init(Default::default)
}

/// Remove the private stages `direct/stages/<slug>` of binaries before the
/// slots (crate::repository_slots), once per process and store — the
/// `go_repository_cache` stage there was the whole GOPATH of an earlier
/// binary (2.5 GB on ~/code). A stage is removed only while its lock
/// `locks/stage-<slug>` can be taken without waiting (no earlier binary
/// executes in it); the lock files stay (removing a lock another process
/// holds would break its exclusion). Best effort: a stage left behind is
/// tried again by the next process. An earlier binary that still runs a
/// rule needing that GOPATH (outside the stage's lock) loses what it held
/// there, as it did when one of its own processes re-executed the cache.
fn sweep_legacy_stages(direct: &Path) {
    static SWEPT: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<PathBuf>>> =
        std::sync::OnceLock::new();
    if !SWEPT
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(direct.to_path_buf())
    {
        return;
    }
    let Ok(entries) = fs::read_dir(direct.join("stages")) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(slug) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        no_follow(&mut options);
        let lock = options.open(direct.join("locks").join(format!("stage-{slug}")));
        let held = lock
            .as_ref()
            .ok()
            .filter(|lock| FileExt::try_lock_exclusive(*lock).is_ok());
        // A stage without a lock file never had a writer of its own.
        let unlocked = lock
            .as_ref()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound);
        if held.is_some() || unlocked {
            let _ = crate::repository_ctx::remove_tree(&entry.path());
        }
        if let Some(lock) = held {
            let _ = FileExt::unlock(lock);
        }
    }
}

/// A finished execution's inventory and its encoded content view (the bytes
/// the identity frames): a repository boundary file is added when missing.
/// With `scan`, the bytes are also searched for store paths
/// (crate::repository_slots::EmbeddedPaths) as they are hashed.
fn stage_content(
    stage: &Path,
    scan: Option<&mut crate::repository_slots::EmbeddedPaths>,
) -> Result<(BTreeMap<String, Entry>, Vec<u8>), String> {
    required_directory(stage)?;
    ensure_boundary(stage)?;
    let files = snapshot_scanned(stage, scan)?;
    let encoded = serde_json::to_vec(&content_view(&files)).map_err(|e| e.to_string())?;
    Ok((files, encoded))
}

/// A warm probe's recorded-input check (`try_reuse`).
type Validate<'a> = &'a dyn Fn(&Path, &[u8]) -> Result<bool, String>;

/// How [`materialize_inner`] treats an existing publication.
enum Probe<'a> {
    /// [`materialize_lazy_in`]: the warm probe's recorded-input check may
    /// reuse it (also through `workspace`'s own pointer).
    Warm {
        validate: Validate<'a>,
        workspace: Option<&'a str>,
    },
    /// [`materialize_local`]: never reused; the execution's runtime
    /// identity parts are reported here.
    Local(&'a mut Option<LocalExecution>),
}

/// The analysis snapshot's `repository-rule-executed` taint of one non-local
/// execution, held until its publication is complete: recorded when dropped
/// (an error after the execution, an execution that did not publish), or
/// settled by [`Self::reprobe`].
struct ExecutedTaint {
    /// `None` once settled without a taint.
    detail: Option<String>,
}

impl ExecutedTaint {
    /// The publication is complete. While the window records a run that
    /// may be saved (crate::observed_fs::reprobes_executions), the warm
    /// probe runs again, exactly as the next invocation's full analysis
    /// would run it ([`try_reuse`]): it reads the specification pointer
    /// (whose outcome before this publication is forgotten: this process
    /// replaced the file), revalidates the manifest's recorded inputs
    /// through the recording facade and claims the verified tree (REPO).
    /// When it reuses `destination` the execution stands as that reuse —
    /// a snapshot saved from this run validates the same pointer, inputs
    /// and tree, so a later hit is valid exactly when that invocation's
    /// full analysis would reuse this publication — and does not taint.
    /// What the execution itself read needs no record: it only shaped the
    /// tree and the recorded inputs, both validated through the probe.
    /// What it printed is the caller's (crate::repository: a printing
    /// execution taints, as the next invocation would print nothing). Any
    /// other outcome (a probe error, another publication, no window, a
    /// verify shadow, a run already tainted otherwise) taints as before.
    ///
    /// `pointer` is the one this publication wrote: the specification's,
    /// or `workspace`'s own for a workspace-dependent publication
    /// ([`workspace_pointer`]), which the re-probe follows like the next
    /// invocation's ([`try_reuse_in`]).
    #[allow(clippy::too_many_arguments)]
    fn reprobe(
        mut self,
        cache: &Path,
        stage_key: &str,
        spec_key: &str,
        workspace: Option<&str>,
        pointer: &Path,
        destination: &Path,
        validate: Validate<'_>,
    ) {
        use crate::observed_fs::{self, Taint};
        if !observed_fs::reprobes_executions()
            || observed_fs::tainted_otherwise(Taint::RepositoryRuleExecuted)
        {
            return;
        }
        observed_fs::supersede(pointer);
        if matches!(
            try_reuse_in(cache, stage_key, spec_key, workspace, validate),
            Ok(Some(found)) if found == destination
        ) {
            self.detail = None;
        }
    }
}

impl Drop for ExecutedTaint {
    fn drop(&mut self) {
        if let Some(detail) = self.detail.take() {
            crate::observed_fs::taint(crate::observed_fs::Taint::RepositoryRuleExecuted, || detail);
        }
    }
}

/// [`materialize_lazy`] or [`materialize_local`] (see [`Probe`]).
fn materialize_inner<F, P>(
    cache: &Path,
    stage_key: &str,
    spec_key: &str,
    provenance: P,
    mut probe: Probe<'_>,
    execute: F,
) -> Result<(PathBuf, Publication), String>
where
    F: FnOnce(&Path) -> Result<Executed, String>,
    P: FnOnce() -> Result<Vec<u8>, String>,
{
    ensure_directory(cache)?;
    let direct = cache.join("direct");
    let trees = direct.join("trees");
    let manifests = direct.join("manifests");
    let locks = direct.join("locks");
    let specs = direct.join("specs");
    for path in [&trees, &manifests, &locks, &specs] {
        ensure_directory(path)?;
    }
    sweep_legacy_stages(&direct);
    // The pointer is published by atomic rename; an unlocked reader sees
    // either the old or the new publication, never a partial one. The warm
    // check is the shared `try_reuse` probe (worker-safe); a local rule
    // never reuses.
    let call_start = timing::enabled().then(std::time::Instant::now);
    if let Probe::Warm {
        validate,
        workspace,
    } = &probe
        && let Some(destination) = try_reuse_in(cache, stage_key, spec_key, *workspace, *validate)?
    {
        if let Some(start) = call_start {
            timing::record_root(stage_key, start.elapsed().as_secs_f64(), "reused");
        }
        return Ok((destination, Publication::Reused));
    }
    // A failing or absent validation falls through to execution, exactly
    // like Bazel refetching when a watched dependency changed. Only now —
    // under the writer lock, after the double-checked reuse below — is the
    // provenance serialized (review R1).
    let provenance = provenance()?;
    let provenance = provenance.as_slice();
    // Held until return: the slot changes only under this lock (its gate
    // first: a waiting writer holds new readers off).
    let writer = crate::repository_slots::SlotWriter::acquire(cache, stage_key)?;
    // A concurrent publisher may have won while this process waited on
    // the writer lock: re-probe before executing (sequential semantics
    // unchanged; the pre-lock probe stays the fast path).
    if let Probe::Warm {
        validate,
        workspace,
    } = &probe
        && let Some(destination) = try_reuse_in(cache, stage_key, spec_key, *workspace, *validate)?
    {
        if let Some(start) = call_start {
            timing::record_root(stage_key, start.elapsed().as_secs_f64(), "reused");
        }
        return Ok((destination, Publication::Reused));
    }
    // Bazel deletes the marker before a fetch (RepositoryFetchFunction.java
    // :311): the slot is incomplete from now on until it is marked again.
    writer.unmark()?;
    let slot = writer.fresh()?;
    // Removes the slot's directory on every early return below.
    let scratch = crate::repository_slots::SlotScratch::new(&writer);
    let started = std::time::Instant::now();
    let executed = {
        let _m = timing::mark(&timing::PHASES.execute);
        let _tainted = crate::observed_fs::tainted_section();
        // `Fetching repository @@name` in the progress display (when on).
        let _fetching = crate::progress::fetching_repository(stage_key);
        execute(&slot)
    };
    // A local rule's execution is recorded for replay by the caller instead.
    // A non-local one taints the analysis snapshot unless the warm probe,
    // run again after the publication below, reuses exactly what was
    // published ([`ExecutedTaint::reprobe`]); every early return taints.
    let mut executed_taint = match &probe {
        Probe::Warm { .. } => Some(ExecutedTaint {
            detail: Some(format!(
                "{stage_key} ({:.1} ms)",
                started.elapsed().as_secs_f64() * 1000.0
            )),
        }),
        Probe::Local(_) => None,
    };
    let executed = executed?;
    // A publication that depends on where the workspace is gets the
    // workspace's own pointer; every other one the specification's.
    let pointer = match recorded_workspace(&executed.recorded) {
        Some(workspace) => specs.join(workspace_pointer(spec_key, &workspace)),
        None => specs.join(spec_key),
    };
    // The tree's bytes are searched for the slot's own path while they are
    // hashed: a self-referencing repository keeps a mutable slot.
    let mut embedded =
        crate::repository_slots::EmbeddedPaths::new(&canonical_store(cache), stage_key);
    let (files, encoded_snapshot) = stage_content(&slot, Some(&mut embedded))?;
    let embedded = embedded.finish();
    let class = if embedded.self_referencing {
        crate::repository_slots::SlotClass::SelfReferencing
    } else {
        crate::repository_slots::SlotClass::Ordinary
    };
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
    let mut manifest = Manifest {
        format: MANIFEST_FORMAT,
        profile: PROFILE.into(),
        identity: identity.clone(),
        files,
        spec: Some(spec_key.to_owned()),
        recorded: Some(executed.recorded),
        self_ref: embedded.self_referencing.then_some(true),
        refs: embedded.refs.into_iter().collect(),
    };
    // A local execution's runtime identity parts, for its replay record.
    if let Probe::Local(execution) = &mut probe {
        **execution = Some(LocalExecution {
            inputs_digest: Sha256::digest(&executed.inputs).into(),
            snapshot_digest: Sha256::digest(&encoded_snapshot).into(),
            identity: identity.clone(),
        });
    }

    // Recheck parents after the callback, before touching persistent entries.
    for path in [&trees, &manifests, &locks, &specs] {
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
    // and recorded probes this binary need not upgrade
    // ([`upgrades_recorded`]) — a manifest written before those fields
    // existed (an older binary's publication), or whose probes an older
    // binary spelled differently (review i20: version-1 probes into the
    // store, which are never trusted), must be upgraded once, or no later
    // invocation could ever validate-and-reuse it through the pointer and
    // the rule would re-execute on every run. A NEWER binary's probes are
    // never replaced. Equal identities record equal probes otherwise (the
    // identity covers the observations they are derived from), so this
    // republishes nothing else; the slot class is a function of the content
    // too, and only a self-referencing manifest without it (a coincidence
    // of an older binary) is upgraded. Only the specification pointer below
    // records this specification's claim.
    if !(read_stable(&manifest_path)
        .ok()
        .and_then(|bytes| parse_manifest(&bytes))
        .is_some_and(|existing| {
            existing.identity == identity
                && content_view(&existing.files) == content_view(&manifest.files)
                && existing.spec.is_some()
                && existing.slot_class() == class
                && !upgrades_recorded(
                    existing.recorded.as_deref(),
                    manifest.recorded.as_deref().unwrap_or_default(),
                )
        })
        && verify_snapshot(&destination, &manifest.files, verify_mode())
            .is_ok_and(|matched| matched))
    {
        // Quarantine names are private to this publisher, and renaming a
        // corrupt symlink never follows it.
        let quarantining = quarantine_lock(&locks, &trees)?;
        let trash = tempfile::Builder::new()
            .prefix(".trash-")
            .tempdir_in(&trees)
            .map(Scratch::new)
            .map_err(|e| e.to_string())?;
        // An ordinary slot becomes the tree (one rename); a self-referencing
        // one stays the repository's mutable directory and a copy of it —
        // fresh single-link files, made inside this publisher's quarantine
        // (removed with it on every exit) — is published, with the copy's
        // own stat fingerprints. Nothing writes into the slot meanwhile: its
        // readers wait for this writer (crate::repository_ctx).
        let tree_source = match class {
            crate::repository_slots::SlotClass::Ordinary => slot.clone(),
            crate::repository_slots::SlotClass::SelfReferencing => {
                let copy = trash.path().join("copy");
                crate::repository_slots::copy_tree(&slot, &copy)?;
                let copied = snapshot(&copy)?;
                if content_view(&copied) != content_view(&manifest.files) {
                    return Err(format!(
                        "the repository directory {} changed while it was published",
                        slot.display()
                    ));
                }
                manifest.files = copied;
                copy
            }
        };
        let encoded = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
        // Prepare metadata before replacing anything.
        let mut metadata_stage = tempfile::NamedTempFile::new_in(&manifests)
            .map_err(|e| format!("cannot stage direct manifest: {e}"))?;
        metadata_stage
            .write_all(&encoded)
            .and_then(|()| metadata_stage.as_file().sync_all())
            .map_err(|e| e.to_string())?;
        quarantine(&manifest_path, &trash.path().join("manifest"))?;
        install_tree(&tree_source, &destination, &trash.path().join("tree"))?;
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
        // The publication is complete: removing the replaced tree (which
        // may hold read-only directories) can no longer fail the command,
        // and never follows symlinks inside our quarantine.
        drop(trash);
        // Explicitly: a child another thread started meanwhile may hold a
        // copy of this descriptor (until its exec), which would keep a
        // closed flock alive and hold off the next sweep.
        let _ = FileExt::unlock(&quarantining);
    }
    // Publish the specification pointer after the tree it names. A crash
    // between them leaves a stale pointer, which the warm path revalidates
    // (manifest, recorded inputs, tree) before reuse, so it can only cost a
    // re-execution, never a wrong reuse.
    {
        let _m = timing::mark(&timing::PHASES.publish);
        publish_pointer(&specs, &pointer, &identity)?;
    }
    // The slot: an ordinary one links the tree (replacing the finished
    // directory if it was not moved into place: an existing equal tree); a
    // self-referencing one is marked complete after the pointer, like
    // genuine's marker file after the repository.
    match class {
        crate::repository_slots::SlotClass::Ordinary => writer.bind(&identity)?,
        crate::repository_slots::SlotClass::SelfReferencing => writer.mark(&identity)?,
    }
    scratch.keep();
    crate::repository_slots::note(cache, stage_key, &identity, class);
    // Released first: the re-probe's manifest refresh coordinates on it.
    drop(lock);
    if let (
        Probe::Warm {
            validate,
            workspace,
        },
        Some(taint),
    ) = (&probe, executed_taint.take())
    {
        taint.reprobe(
            cache,
            stage_key,
            spec_key,
            *workspace,
            &pointer,
            &destination,
            *validate,
        );
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

/// Atomically install the specification pointer naming `identity`; a
/// pointer already holding exactly these bytes is left as it is (no new
/// inode for unchanged content: re-executed repositories would otherwise
/// churn the pointers an analysis snapshot validates, PLAN-L2 update 1
/// §(c)5).
fn publish_pointer(specs: &Path, pointer: &Path, identity: &str) -> Result<(), String> {
    if read_stable(pointer).is_ok_and(|bytes| bytes == pointer_bytes(identity).as_bytes()) {
        return Ok(());
    }
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
/// Unknown fields are tolerated (review i20): only `identity` is ever used,
/// and only after the strict checks below — `format` exactly 1 and a
/// 64-digit lowercase hex identity, which is joined into store paths — and
/// the warm probe then requires the manifest it names to carry the same
/// identity and revalidates recorded inputs and tree before any reuse. A
/// newer binary's additive pointer field therefore cannot change what is
/// reused, where rejecting it would fail this binary's builds outright.
fn read_pointer(path: &Path) -> Result<Option<String>, String> {
    #[derive(Deserialize)]
    struct Pointer {
        format: u32,
        identity: String,
    }
    // A missing pointer is a specification never published: no waiting.
    let Ok(bytes) = read_published(path, false) else {
        return Ok(None);
    };
    crate::observed_fs::note_read(path, &bytes);
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

/// Put the finished `stage` in place as `destination`. An existing
/// destination (a republication: a corrupt tree of this identity, or an
/// intact one whose manifest is upgraded) is swapped with the stage in one
/// step, `renameat2(RENAME_EXCHANGE)`, so that a reader in another process —
/// an action running through a link into the tree (crate::action
/// `stage_tree_link`), a command reading the repository — always finds a
/// tree at that path, never none; the replaced tree, now at the stage path,
/// then moves into the publisher's quarantine `trash` (if that fails, the
/// stage's own cleanup removes it there). A filesystem that cannot exchange
/// gets the quarantine first and the stage renamed after it, which leaves
/// the path briefly without a tree.
fn install_tree(stage: &Path, destination: &Path, trash: &Path) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    if optional_metadata(destination)?.is_some()
        && rustix::fs::renameat_with(
            rustix::fs::CWD,
            stage,
            rustix::fs::CWD,
            destination,
            rustix::fs::RenameFlags::EXCHANGE,
        )
        .is_ok()
    {
        let _ = fs::rename(stage, trash);
        return Ok(());
    }
    quarantine(destination, trash)?;
    fs::rename(stage, destination).map_err(|e| format!("cannot publish direct repository: {e}"))
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

/// How often [`read_published`] reads a file that is being replaced.
const PUBLISHED_READ_ATTEMPTS: u32 = 8;

/// A test hook run with the number of the attempt that failed.
#[cfg(test)]
type RetryHook = Box<dyn FnMut(u32)>;

#[cfg(test)]
thread_local! {
    /// Test hook: runs before each further attempt of [`read_published`]
    /// on this thread.
    static BEFORE_PUBLISHED_RETRY: std::cell::RefCell<Option<RetryHook>> =
        const { std::cell::RefCell::new(None) };
}

/// A publication's specification pointer or manifest, read for the warm
/// probe ([`try_reuse`]). Publishers and the probes of other threads and
/// processes replace these files by renames while a probe reads them
/// ([`publish_pointer`], [`refresh_manifest`]; a republication renames the
/// old manifest away before it renames the new one in), so a read that
/// checks the path again after reading ([`read_stable`]) is torn by a
/// rename and missed a valid publication. Here the bytes come from the one
/// opened file, complete whatever is renamed over its path meanwhile; only
/// a change of that file itself while it is read fails an attempt. Such an
/// attempt, and with `wait_if_missing` a missing file (after a pause of
/// 1 ms, then 2, ..., 28 ms in all), is tried again, up to
/// [`PUBLISHED_READ_ATTEMPTS`] times; the last error stands.
fn read_published(path: &Path, wait_if_missing: bool) -> Result<Vec<u8>, String> {
    let mut attempt = 1;
    loop {
        let (missing, error) = match read_opened(path) {
            Ok(bytes) => return Ok(bytes),
            Err(Unread::Missing(error)) if wait_if_missing => (true, error),
            Err(Unread::Changed(error)) => (false, error),
            Err(Unread::Missing(error) | Unread::Failed(error)) => return Err(error),
        };
        if attempt == PUBLISHED_READ_ATTEMPTS {
            return Err(error);
        }
        #[cfg(test)]
        BEFORE_PUBLISHED_RETRY.with(|hook| {
            if let Some(hook) = hook.borrow_mut().as_mut() {
                hook(attempt);
            }
        });
        if missing {
            std::thread::sleep(std::time::Duration::from_millis(u64::from(attempt)));
        }
        attempt += 1;
    }
}

/// Why [`read_opened`] read nothing.
enum Unread {
    /// No file at the path.
    Missing(String),
    /// The opened file changed while it was read.
    Changed(String),
    Failed(String),
}

/// One attempt of [`read_published`]: the regular file `path` (never
/// through a link, never hard-linked) read whole from one descriptor, which
/// must keep its size and modification time while it is read. A rename over
/// the path only unlinks it: no link left is no second link.
fn read_opened(path: &Path) -> Result<Vec<u8>, Unread> {
    let failed = |error: std::io::Error| Unread::Failed(error.to_string());
    let mut options = OpenOptions::new();
    options.read(true);
    no_follow(&mut options);
    let mut file = options.open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Unread::Missing(error.to_string())
        } else {
            Unread::Failed(error.to_string())
        }
    })?;
    let opened = file.metadata().map_err(failed)?;
    #[cfg(unix)]
    let unlinked = {
        use std::os::unix::fs::MetadataExt;
        opened.nlink() == 0
    };
    #[cfg(not(unix))]
    let unlinked = false;
    if !(opened.is_file() && unlinked) {
        regular_metadata(&opened).map_err(Unread::Failed)?;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(failed)?;
    let read = file.metadata().map_err(failed)?;
    if bytes.len() as u64 != opened.len()
        || read.len() != opened.len()
        || read.modified().ok() != opened.modified().ok()
    {
        return Err(Unread::Changed(
            "direct repository file changed while reading".into(),
        ));
    }
    Ok(bytes)
}

/// SHA-256 and size of a regular file, hashed incrementally (constant memory).
fn hash_stable(path: &Path) -> Result<(u64, String), String> {
    hash_scanned(path, None)
}

/// [`hash_stable`], the same bytes also searched with `scan`.
fn hash_scanned(
    path: &Path,
    scan: Option<&mut crate::repository_slots::EmbeddedPaths>,
) -> Result<(u64, String), String> {
    /// Both sinks of one read.
    struct Tee<'a, 'b>(
        &'a mut Sha256,
        &'a mut crate::repository_slots::FileScan<'b>,
    );
    impl Write for Tee<'_, '_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            self.1.write_all(bytes)?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut digest = Sha256::new();
    let size = match scan {
        None => stream_stable(path, &mut digest)?,
        Some(paths) => {
            let mut file = paths.file();
            let size = stream_stable(path, &mut Tee(&mut digest, &mut file))?;
            file.finish();
            size
        }
    };
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

/// The fingerprint `metadata` may carry in a manifest recorded at
/// `recorded_at` (review R3): only a file already trusted at recording
/// time — mtime AND ctime at least the racy margin (2s) older than
/// `recorded_at` — may be fingerprinted. A younger file could still be
/// rewritten with the same coarse timestamps; it gets NO fingerprint
/// and is hashed on warm verify (whose match then refreshes it).
fn settled_fingerprint(
    metadata: &fs::Metadata,
    recorded_at: std::time::SystemTime,
) -> Option<StatFingerprint> {
    let fp = fingerprint(metadata);
    racy_safe(&fp, recorded_at).then_some(fp)
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
    Ok(verify_snapshot_detailed(root, expected, mode)?.matched)
}

/// Outcome of a warm verification: whether the tree matches, and the
/// refreshed fingerprints for entries whose fallback hash matched but
/// whose recorded fingerprint was missing/stale/racy (review R3: the
/// caller persists them so the next warm run trusts by stat).
#[derive(Debug)]
struct Verified {
    matched: bool,
    refreshed: BTreeMap<String, StatFingerprint>,
    /// Regular files this verification hashed. Tests assert on it: the
    /// process-wide `files_hashed` counter is shared with concurrently
    /// running tests (review i20: a concurrent test's hashes made a delta
    /// assertion on it flaky).
    #[cfg_attr(not(test), allow(dead_code))]
    hashed: u64,
}

fn verify_snapshot_detailed(
    root: &Path,
    expected: &BTreeMap<String, Entry>,
    mode: VerifyMode,
) -> Result<Verified, String> {
    verify_snapshot_at(root, expected, mode, std::time::SystemTime::now())
}

/// [`verify_snapshot_detailed`] with the racy-window reference instant
/// injected (production: now; tests: a settled future, deterministically).
fn verify_snapshot_at(
    root: &Path,
    expected: &BTreeMap<String, Entry>,
    mode: VerifyMode,
    start: std::time::SystemTime,
) -> Result<Verified, String> {
    if mode == VerifyMode::Full {
        let files = snapshot(root)?;
        return Ok(Verified {
            matched: content_view(&files) == content_view(expected),
            refreshed: BTreeMap::new(),
            hashed: files
                .values()
                .filter(|entry| !entry.directory && entry.symlink.is_none())
                .count() as u64,
        });
    }
    fn visit(
        root: &Path,
        path: &Path,
        expected: &BTreeMap<String, Entry>,
        seen: &mut std::collections::BTreeSet<String>,
        start: std::time::SystemTime,
        refreshed: &mut BTreeMap<String, StatFingerprint>,
        hashed: &mut u64,
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
            return Ok(want.symlink.as_deref() == Some(target) && want.size == target.len() as u64);
        }
        if before.is_dir() {
            if !want.directory || mode_of(&before)? != want.mode {
                return Ok(false);
            }
            for child in fs::read_dir(path).map_err(|e| e.to_string())? {
                if !visit(
                    root,
                    &child.map_err(|e| e.to_string())?.path(),
                    expected,
                    seen,
                    start,
                    refreshed,
                    hashed,
                )? {
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
        let current = fingerprint(&before);
        let trusted = want
            .stat
            .is_some_and(|fp| fp == current && racy_safe(&fp, start));
        if trusted {
            return Ok(true);
        }
        timing::PHASES
            .files_hashed
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        *hashed += 1;
        let (_, sha256) = hash_stable(path)?;
        if sha256 != want.sha256 {
            return Ok(false);
        }
        // Hash matched: refresh the fingerprint (review R3) when the
        // current stat is settled relative to THIS verify's start, so an
        // external ctime bump (chmod/backup) doesn't re-hash forever.
        if racy_safe(&current, start) && want.stat != Some(current) {
            refreshed.insert(name, current);
        }
        Ok(true)
    }
    required_directory(root)?;
    let mut seen = std::collections::BTreeSet::new();
    let mut refreshed = BTreeMap::new();
    let mut hashed = 0;
    if !visit(
        root,
        root,
        expected,
        &mut seen,
        start,
        &mut refreshed,
        &mut hashed,
    )? {
        return Ok(Verified {
            matched: false,
            refreshed: BTreeMap::new(),
            hashed,
        });
    }
    Ok(Verified {
        matched: seen.len() == expected.len(),
        refreshed,
        hashed,
    })
}

fn mode_of(metadata: &fs::Metadata) -> Result<u32, String> {
    mode(metadata)
}

/// A published tree of the native repository store, any of the four tree
/// areas: registry archives (`<store>/trees/<id>`, manifest
/// `<store>/trees/<id>.json`, crate::materializer), generated repositories
/// (`<store>/direct/trees/<id>`, manifest `direct/manifests/<id>.json`),
/// Git sources (`<store>/git/trees/<id>`, `git/manifests/<id>.json`) and
/// the embedded `@bazel_tools` (`<store>/builtins/trees/<id>`,
/// `builtins/trees/<id>.json`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedTree {
    /// `registry`, `direct`, `git` or `builtin` (the REPO record kinds).
    pub kind: &'static str,
    pub id: String,
    pub root: PathBuf,
    pub manifest: PathBuf,
}

/// The published store tree containing `path`, if any (lexically: `store`
/// and `path` spelled alike, e.g. both canonical): `path` lies in
/// `<store>/<area>/<id>` for one of the four tree areas, `<id>` is a
/// publication identity — 64 lowercase hex digits, never a stage, trash,
/// lock or manifest name — and the tree is a directory with a regular
/// manifest file (PLAN-L2 update 1 §(c): such a tree is a link anchor in
/// any order).
pub fn published_tree(store: &Path, path: &Path) -> Option<PublishedTree> {
    store_tree_of(store, path).filter(is_published)
}

/// Whether `tree` ([`store_tree_of`]) is published: a directory with a
/// regular manifest file (two lstats).
pub fn is_published(tree: &PublishedTree) -> bool {
    fs::symlink_metadata(&tree.root).is_ok_and(|metadata| metadata.is_dir())
        && fs::symlink_metadata(&tree.manifest).is_ok_and(|metadata| metadata.is_file())
}

/// The store tree `path` lexically lies in, published or not (no I/O; see
/// [`published_tree`]): its area, identity, root and manifest path.
pub fn store_tree_of(store: &Path, path: &Path) -> Option<PublishedTree> {
    let rest = path.strip_prefix(store).ok()?;
    let mut parts = rest.components().map(|component| match component {
        Component::Normal(part) => part.to_str(),
        _ => None,
    });
    let (kind, area) = match parts.next()?? {
        "trees" => ("registry", store.join("trees")),
        area @ ("direct" | "git" | "builtins") => {
            if parts.next()?? != "trees" {
                return None;
            }
            let kind = match area {
                "direct" => "direct",
                "git" => "git",
                _ => "builtin",
            };
            (kind, store.join(area).join("trees"))
        }
        _ => return None,
    };
    let id = parts.next()??;
    if id.len() != 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let root = area.join(id);
    let manifest = match kind {
        "direct" => store.join("direct/manifests").join(format!("{id}.json")),
        "git" => store.join("git/manifests").join(format!("{id}.json")),
        _ => area.join(format!("{id}.json")),
    };
    Some(PublishedTree {
        kind,
        id: id.to_owned(),
        root,
        manifest,
    })
}

/// What a published tree's manifest records for one entry below its root,
/// whatever store wrote it: content only — kind, file digest, link target —
/// never modes, sizes or stat fingerprints, which do not change what
/// analysis reads.
#[derive(Debug, PartialEq, Eq)]
enum TreeContent {
    Directory,
    File(String),
    Symlink(String),
}

/// Verify a published tree against its manifest's content (a link crossing
/// into a tree the invocation did not materialize, crate::source_links):
/// exactly the recorded entries below the root, each with the recorded
/// kind, SHA-256 or link target. Never follows links; reads every file once.
pub fn verify_published_tree(tree: &PublishedTree) -> Result<(), String> {
    check_published_tree(tree).map_err(|mismatch| mismatch.message)
}

/// [`verify_published_tree`] naming the first differing entry (L2-W5h: the
/// analysis snapshot's healing checks a crossed tree this way).
pub fn check_published_tree(tree: &PublishedTree) -> Result<(), TreeMismatch> {
    let manifest_error =
        |message: String| TreeMismatch::manifest(&tree.manifest, &tree.root, message);
    let bytes = read_stable(&tree.manifest).map_err(manifest_error)?;
    let manifest: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| manifest_error(format!("cannot parse {}: {e}", tree.manifest.display())))?;
    let expected = manifest_content(tree.kind, &manifest).ok_or_else(|| {
        manifest_error(format!(
            "malformed repository manifest {}",
            tree.manifest.display()
        ))
    })?;
    let mut actual = BTreeMap::new();
    let walked = tree_content(&tree.root, &tree.root, &mut actual);
    if walked.is_err() || actual != expected {
        // The content check: modes do not count (a crossing reads content).
        let mut found = manifest_inventory(tree.kind, &manifest)
            .map(|recorded| diagnose_tree(&tree.root, &recorded, false))
            .unwrap_or_else(|| TreeMismatch::other(&tree.root));
        found.message = format!(
            "published repository tree {} does not match its manifest ({})",
            tree.root.display(),
            found.entry_text()
        );
        return Err(found);
    }
    Ok(())
}

/// Where a published tree first differs from what its manifest records
/// (L2-W5h: the analysis snapshot's healing and a hit's check of a local
/// repository's tree name it, crate::snapshot::hit and
/// crate::snapshot::telemetry).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeMismatch {
    /// `extra` (an entry the manifest does not record), `missing`, `kind`
    /// (a file, directory or link replaced by another kind), `mode`,
    /// `content` (another size or digest), `link` (another link target),
    /// `manifest` (missing, unreadable or describing another
    /// publication), `unreadable` (an entry that cannot be inspected or
    /// read) or `other` (no single entry differs on a second look: a
    /// concurrent change, or what only the store's own check rejects).
    pub class: &'static str,
    /// The differing entry (absolute), the manifest or the tree root.
    pub path: PathBuf,
    /// The tree's root.
    pub root: PathBuf,
    /// What differs, for humans.
    pub message: String,
}

impl std::fmt::Display for TreeMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl TreeMismatch {
    /// The entry `name` (relative to `root`) differs as `class`.
    fn entry(root: &Path, class: &'static str, name: &str) -> Self {
        let path = if name.is_empty() {
            root.to_path_buf()
        } else {
            root.join(name)
        };
        let mut mismatch = Self {
            class,
            path,
            root: root.to_path_buf(),
            message: String::new(),
        };
        mismatch.message = format!(
            "repository tree {} does not match its manifest ({})",
            root.display(),
            mismatch.entry_text()
        );
        mismatch
    }

    /// The manifest at `manifest` does not describe the tree at `root`.
    fn manifest(manifest: &Path, root: &Path, message: String) -> Self {
        Self {
            class: "manifest",
            path: manifest.to_path_buf(),
            root: root.to_path_buf(),
            message,
        }
    }

    /// The tree at `root` differs, but no single entry does.
    fn other(root: &Path) -> Self {
        Self::entry(root, "other", "")
    }

    /// `<class> <path relative to the root>` (`.` for the root itself).
    pub fn entry_text(&self) -> String {
        let relative = self
            .path
            .strip_prefix(&self.root)
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
            .map_or_else(
                || self.path.display().to_string(),
                |relative| relative.display().to_string(),
            );
        format!("{} {relative}", self.class)
    }
}

/// What a manifest records for one entry below a tree's root, whatever
/// store wrote it, as [`diagnose_tree`] compares it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Recorded {
    Directory {
        mode: Option<u32>,
    },
    File {
        mode: Option<u32>,
        size: Option<u64>,
        sha256: String,
    },
    Symlink(String),
}

/// A manifest's entries below the root ([`Recorded`]): the registry
/// archives' and this store's (directory flag, mode, size, digest, link
/// target), Git's (kind, mode, size, digest) and the embedded
/// `@bazel_tools`' (a file list with sizes and modes; its directories are
/// implied, mode 0755).
fn manifest_inventory(
    kind: &str,
    manifest: &serde_json::Value,
) -> Option<BTreeMap<String, Recorded>> {
    use serde_json::Value;
    let text = |value: &Value| value.as_str().map(str::to_owned);
    let mode = |entry: &Value| {
        entry
            .get("mode")
            .and_then(Value::as_u64)
            .and_then(|mode| u32::try_from(mode).ok())
    };
    let size = |entry: &Value| entry.get("size").and_then(Value::as_u64);
    let mut recorded = BTreeMap::new();
    match kind {
        "builtin" => {
            for entry in manifest.get("files")?.as_array()? {
                let path = entry.get("path")?.as_str()?;
                for (index, _) in path.match_indices('/') {
                    recorded.insert(
                        path[..index].to_owned(),
                        Recorded::Directory { mode: Some(0o755) },
                    );
                }
                recorded.insert(
                    path.to_owned(),
                    Recorded::File {
                        mode: mode(entry),
                        size: size(entry),
                        sha256: text(entry.get("sha256")?)?,
                    },
                );
            }
        }
        "git" => {
            for (name, entry) in manifest.get("files")?.as_object()? {
                if name.is_empty() {
                    continue;
                }
                let value = match entry.get("kind")? {
                    Value::String(kind) if kind == "Directory" => {
                        Recorded::Directory { mode: mode(entry) }
                    }
                    Value::String(kind) if kind == "File" => Recorded::File {
                        mode: mode(entry),
                        size: size(entry),
                        sha256: text(entry.get("sha256")?)?,
                    },
                    Value::Object(link) => {
                        Recorded::Symlink(text(link.get("Symlink")?.get("target")?)?)
                    }
                    _ => return None,
                };
                recorded.insert(name.clone(), value);
            }
        }
        _ => {
            for (name, entry) in manifest.get("files")?.as_object()? {
                if name.is_empty() {
                    continue;
                }
                let value = if let Some(target) = entry.get("symlink").and_then(Value::as_str) {
                    Recorded::Symlink(target.to_owned())
                } else if entry.get("directory")?.as_bool()? {
                    Recorded::Directory { mode: mode(entry) }
                } else {
                    Recorded::File {
                        mode: mode(entry),
                        size: size(entry),
                        sha256: text(entry.get("sha256")?)?,
                    }
                };
                recorded.insert(name.clone(), value);
            }
        }
    }
    Some(recorded)
}

/// The first entry where the tree at `root` differs from `recorded`, after
/// the store's own check found that it does (L2-W5h; only then: it reads
/// the tree again). Structure first — an extra or missing entry, another
/// kind, link target, mode (with `modes`) or size, in path order — then
/// the files' contents in path order, so a tampered tree costs at most a
/// walk and the hashing up to the first differing file. Never follows
/// links.
fn diagnose_tree(root: &Path, recorded: &BTreeMap<String, Recorded>, modes: bool) -> TreeMismatch {
    #[derive(Debug)]
    enum Actual {
        Directory(u32),
        File(u32, u64),
        Symlink(String),
        Other,
    }
    fn walk(
        root: &Path,
        directory: &Path,
        actual: &mut BTreeMap<String, Actual>,
    ) -> Result<(), PathBuf> {
        let entries = fs::read_dir(directory).map_err(|_| directory.to_path_buf())?;
        for entry in entries {
            let path = entry.map_err(|_| directory.to_path_buf())?.path();
            let name = path
                .strip_prefix(root)
                .map_err(|_| path.clone())?
                .to_string_lossy()
                .into_owned();
            let metadata = fs::symlink_metadata(&path).map_err(|_| path.clone())?;
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o7777
            };
            #[cfg(not(unix))]
            let mode = 0;
            if metadata.file_type().is_symlink() {
                let target = fs::read_link(&path).map_err(|_| path.clone())?;
                actual.insert(name, Actual::Symlink(target.to_string_lossy().into_owned()));
            } else if metadata.is_dir() {
                actual.insert(name, Actual::Directory(mode));
                walk(root, &path, actual)?;
            } else if metadata.is_file() {
                actual.insert(name, Actual::File(mode, metadata.len()));
            } else {
                actual.insert(name, Actual::Other);
            }
        }
        Ok(())
    }
    if !fs::symlink_metadata(root).is_ok_and(|metadata| metadata.is_dir()) {
        let class = if fs::symlink_metadata(root).is_ok() {
            "kind"
        } else {
            "missing"
        };
        return TreeMismatch::entry(root, class, "");
    }
    let mut actual = BTreeMap::new();
    if let Err(path) = walk(root, root, &mut actual) {
        let name = path
            .strip_prefix(root)
            .map(|relative| relative.to_string_lossy().into_owned())
            .unwrap_or_default();
        return TreeMismatch::entry(root, "unreadable", &name);
    }
    // Structure, in path order (both maps are sorted by name).
    let mut actual_entries = actual.iter().peekable();
    let mut recorded_entries = recorded.iter().peekable();
    loop {
        let (name, class) = match (actual_entries.peek(), recorded_entries.peek()) {
            (None, None) => break,
            (Some((name, _)), None) => (name.as_str(), Some("extra")),
            (None, Some((name, _))) => (name.as_str(), Some("missing")),
            (Some((have_name, _)), Some((want_name, _))) if have_name < want_name => {
                (have_name.as_str(), Some("extra"))
            }
            (Some((have_name, _)), Some((want_name, _))) if have_name > want_name => {
                (want_name.as_str(), Some("missing"))
            }
            (Some((name, have)), Some((_, want))) => {
                let mode_differs =
                    |have: u32, want: Option<u32>| modes && want.is_some_and(|want| want != have);
                let class = match (have, want) {
                    (Actual::Symlink(have), Recorded::Symlink(want)) => {
                        (have != want).then_some("link")
                    }
                    (Actual::Directory(have), Recorded::Directory { mode }) => {
                        mode_differs(*have, *mode).then_some("mode")
                    }
                    (Actual::File(have_mode, have_size), Recorded::File { mode, size, .. }) => {
                        if mode_differs(*have_mode, *mode) {
                            Some("mode")
                        } else if size.is_some_and(|size| size != *have_size) {
                            Some("content")
                        } else {
                            None
                        }
                    }
                    _ => Some("kind"),
                };
                (name.as_str(), class)
            }
        };
        if let Some(class) = class {
            return TreeMismatch::entry(root, class, name);
        }
        actual_entries.next();
        recorded_entries.next();
    }
    // Contents, in path order.
    for (name, want) in recorded {
        if let Recorded::File { sha256, .. } = want {
            let path = root.join(name);
            let mut options = OpenOptions::new();
            options.read(true);
            no_follow(&mut options);
            let Ok(mut file) = options.open(&path) else {
                return TreeMismatch::entry(root, "unreadable", name);
            };
            let mut digest = Sha256::new();
            if std::io::copy(&mut file, &mut digest).is_err() {
                return TreeMismatch::entry(root, "unreadable", name);
            }
            if format!("{:x}", digest.finalize()) != *sha256 {
                return TreeMismatch::entry(root, "content", name);
            }
        }
    }
    TreeMismatch::other(root)
}

/// [`diagnose_tree`] of a published store tree against its manifest file
/// (an unreadable or malformed manifest is the difference).
fn diagnose_published(tree: &PublishedTree) -> TreeMismatch {
    let manifest = fs::read(&tree.manifest)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let Some(manifest) = manifest else {
        return TreeMismatch::manifest(
            &tree.manifest,
            &tree.root,
            format!(
                "repository tree {} does not match its manifest (cannot read {})",
                tree.root.display(),
                tree.manifest.display()
            ),
        );
    };
    match manifest_inventory(tree.kind, &manifest) {
        Some(recorded) => match diagnose_tree(&tree.root, &recorded, true) {
            // Every entry matches: the store's check rejected the manifest
            // itself (its format, policy or the publication it names).
            found if found.class == "other" => TreeMismatch::manifest(
                &tree.manifest,
                &tree.root,
                format!(
                    "repository tree {} does not match its manifest (the manifest {} describes another publication)",
                    tree.root.display(),
                    tree.manifest.display()
                ),
            ),
            found => found,
        },
        None => TreeMismatch::manifest(
            &tree.manifest,
            &tree.root,
            format!(
                "repository tree {} does not match its manifest (malformed manifest {})",
                tree.root.display(),
                tree.manifest.display()
            ),
        ),
    }
}

/// Verify a published store tree the way its store's warm check does before
/// it reuses the tree (L2-W5g: a failed analysis-snapshot hit's healing,
/// crate::snapshot::hit) — content, modes and links, trusted by stat where
/// that check trusts, every file read under `--verify_repo_tampering`: a
/// direct tree against its manifest (this store's
/// [`verify_snapshot`]), a registry archive (crate::materializer), a Git
/// source (crate::git_source) and the embedded `@bazel_tools`
/// (crate::builtin_tools) against theirs. Reads only. The error names the
/// first differing entry (an unreadable tree or manifest is one; L2-W5h).
pub fn verify_as_reused(tree: &PublishedTree) -> Result<(), TreeMismatch> {
    let matched = match tree.kind {
        "direct" => verify_direct_publication(tree).unwrap_or(false),
        "registry" => crate::materializer::published_tree_matches(&tree.root, &tree.manifest),
        "git" => crate::git_source::published_tree_matches(&tree.root, &tree.manifest, &tree.id),
        "builtin" => crate::builtin_tools::published_tree_matches(&tree.root, &tree.manifest),
        kind => {
            return Err(TreeMismatch::manifest(
                &tree.manifest,
                &tree.root,
                format!("unknown repository tree kind {kind}"),
            ));
        }
    };
    if matched {
        Ok(())
    } else {
        Err(diagnose_published(tree))
    }
}

/// A direct tree's warm check ([`try_reuse`] without the recorded inputs:
/// the analysis snapshot validated those): its manifest names this
/// publication and the tree verifies against it.
fn verify_direct_publication(tree: &PublishedTree) -> Result<bool, String> {
    let bytes = read_stable(&tree.manifest)
        .map_err(|error| format!("cannot read {}: {error}", tree.manifest.display()))?;
    let Some(manifest) = parse_manifest(&bytes) else {
        return Ok(false);
    };
    if manifest.identity != tree.id {
        return Ok(false);
    }
    verify_snapshot(&tree.root, &manifest.files, verify_mode())
        .map_err(|error| format!("cannot verify {}: {error}", tree.root.display()))
}

/// A local repository's published tree still holds what its execution
/// produced (L2-W5g: a failed analysis-snapshot hit's healing — the hit
/// replays the rule in a fresh stage and never looks into the published
/// tree): the manifest beside it records the execution's identity and tree
/// digest, and the tree verifies against that manifest as
/// [`materialize_local`]'s republication check does (content, modes, links;
/// trusted by stat outside the racy window; every file read under
/// `--verify_repo_tampering`). Reads only. The error names the first
/// differing entry (L2-W5h).
pub fn verify_local_publication(
    root: &Path,
    execution: &LocalExecution,
) -> Result<(), TreeMismatch> {
    let identity = &execution.identity;
    let manifest_path = root
        .parent()
        .and_then(Path::parent)
        .filter(|_| root.file_name().is_some_and(|name| *name == **identity))
        .map(|direct| direct.join("manifests").join(format!("{identity}.json")))
        .ok_or_else(|| {
            TreeMismatch::manifest(
                root,
                root,
                format!(
                    "{} is not the publication {identity} of the direct store",
                    root.display()
                ),
            )
        })?;
    let manifest_error = |message: String| TreeMismatch::manifest(&manifest_path, root, message);
    let bytes = read_stable(&manifest_path).map_err(|error| {
        manifest_error(format!("cannot read {}: {error}", manifest_path.display()))
    })?;
    let manifest = parse_manifest(&bytes).ok_or_else(|| {
        manifest_error(format!("unreadable manifest {}", manifest_path.display()))
    })?;
    let encoded = serde_json::to_vec(&content_view(&manifest.files))
        .map_err(|error| manifest_error(error.to_string()))?;
    let digest: [u8; 32] = Sha256::digest(&encoded).into();
    if manifest.identity != *identity || digest != execution.snapshot_digest {
        return Err(manifest_error(format!(
            "the manifest {} does not record this execution's tree",
            manifest_path.display()
        )));
    }
    if verify_snapshot(root, &manifest.files, verify_mode()).unwrap_or(false) {
        return Ok(());
    }
    let recorded = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .and_then(|value| manifest_inventory("direct", &value));
    Err(match recorded {
        Some(recorded) => diagnose_tree(root, &recorded, true),
        None => TreeMismatch::other(root),
    })
}

fn manifest_content(
    kind: &str,
    manifest: &serde_json::Value,
) -> Option<BTreeMap<String, TreeContent>> {
    use serde_json::Value;
    let text = |value: &Value| value.as_str().map(str::to_owned);
    let mut expected = BTreeMap::new();
    match kind {
        // builtin_tools: a file list; directories are implied.
        "builtin" => {
            for entry in manifest.get("files")?.as_array()? {
                let path = entry.get("path")?.as_str()?;
                for (index, _) in path.match_indices('/') {
                    expected.insert(path[..index].to_owned(), TreeContent::Directory);
                }
                expected.insert(
                    path.to_owned(),
                    TreeContent::File(text(entry.get("sha256")?)?),
                );
            }
        }
        // git_source: `kind` is "Directory", "File" or {"Symlink": {"target"}}.
        "git" => {
            for (name, entry) in manifest.get("files")?.as_object()? {
                if name.is_empty() {
                    continue;
                }
                let content = match entry.get("kind")? {
                    Value::String(kind) if kind == "Directory" => TreeContent::Directory,
                    Value::String(kind) if kind == "File" => {
                        TreeContent::File(text(entry.get("sha256")?)?)
                    }
                    Value::Object(link) => {
                        TreeContent::Symlink(text(link.get("Symlink")?.get("target")?)?)
                    }
                    _ => return None,
                };
                expected.insert(name.clone(), content);
            }
        }
        // materializer and this store: directory flag, digest, link target.
        _ => {
            for (name, entry) in manifest.get("files")?.as_object()? {
                if name.is_empty() {
                    continue;
                }
                let content = if let Some(target) = entry.get("symlink").and_then(Value::as_str) {
                    TreeContent::Symlink(target.to_owned())
                } else if entry.get("directory")?.as_bool()? {
                    TreeContent::Directory
                } else {
                    TreeContent::File(text(entry.get("sha256")?)?)
                };
                expected.insert(name.clone(), content);
            }
        }
    }
    Some(expected)
}

fn tree_content(
    root: &Path,
    directory: &Path,
    content: &mut BTreeMap<String, TreeContent>,
) -> Result<(), String> {
    for child in fs::read_dir(directory).map_err(|e| e.to_string())? {
        let path = child.map_err(|e| e.to_string())?.path();
        let name = path
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .to_str()
            .ok_or("non-UTF-8 repository tree path")?
            .to_owned();
        let metadata = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path).map_err(|e| e.to_string())?;
            let target = target
                .to_str()
                .ok_or("non-UTF-8 repository symlink target")?
                .to_owned();
            content.insert(name, TreeContent::Symlink(target));
        } else if metadata.is_dir() {
            content.insert(name, TreeContent::Directory);
            tree_content(root, &path, content)?;
        } else if metadata.is_file() {
            let mut options = OpenOptions::new();
            options.read(true);
            no_follow(&mut options);
            let mut file = options.open(&path).map_err(|e| e.to_string())?;
            let mut digest = Sha256::new();
            std::io::copy(&mut file, &mut digest).map_err(|e| e.to_string())?;
            content.insert(name, TreeContent::File(format!("{:x}", digest.finalize())));
        } else {
            return Err(format!(
                "unsupported file type in repository tree: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn snapshot(root: &Path) -> Result<BTreeMap<String, Entry>, String> {
    snapshot_scanned(root, None)
}

/// [`snapshot`], searching every file's bytes (as they are hashed) and every
/// link target with `scan`.
fn snapshot_scanned(
    root: &Path,
    mut scan: Option<&mut crate::repository_slots::EmbeddedPaths>,
) -> Result<BTreeMap<String, Entry>, String> {
    // Recording moment for fingerprint settling (review R3): taken once
    // before the walk; files younger than the racy margin get no stat.
    let recorded_at = std::time::SystemTime::now();
    fn visit(
        root: &Path,
        path: &Path,
        files: &mut BTreeMap<String, Entry>,
        recorded_at: std::time::SystemTime,
        scan: &mut Option<&mut crate::repository_slots::EmbeddedPaths>,
    ) -> Result<(), String> {
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
            if let Some(scan) = scan.as_deref_mut() {
                scan.link(target.as_bytes());
            }
            files.insert(
                name,
                Entry {
                    directory: false,
                    mode: 0,
                    size: target.len() as u64,
                    sha256: format!("{:x}", Sha256::digest(target.as_bytes())),
                    symlink: Some(target),
                    stat: settled_fingerprint(&before, recorded_at),
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
            stat: settled_fingerprint(&before, recorded_at),
        };
        if !entry.directory {
            regular_metadata(&before)?;
            let (size, sha256) = hash_scanned(path, scan.as_deref_mut())?;
            entry.size = size;
            entry.sha256 = sha256;
        }
        let directory = entry.directory;
        files.insert(name, entry);
        if directory {
            for child in fs::read_dir(path).map_err(|e| e.to_string())? {
                visit(
                    root,
                    &child.map_err(|e| e.to_string())?.path(),
                    files,
                    recorded_at,
                    scan,
                )?;
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
    visit(root, root, &mut files, recorded_at, &mut scan)?;
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

    /// Review R1 equivalence: the streamed digest-entry bytes are
    /// byte-identical to the `Value` form, and the streamed spec identity
    /// (count + seeded hash) equals `spec_digest_identity` — so pointers
    /// published before the streaming switch still hit.
    #[test]
    fn source_digest_streaming_matches_value_form() {
        let mut sources = BTreeMap::new();
        for (label, bytes, mapping) in [
            ("//a:x.bzl", &b"one"[..], [("m", "n")].as_slice()),
            ("@r//b:y.bzl", b"two \xff\xfe", [].as_slice()),
            ("//z.bzl", b"", [("a\"q", "b\\c"), ("é", "ü")].as_slice()),
        ] {
            sources.insert(
                label.to_owned(),
                crate::loading::SourceProvenance {
                    bytes: bytes.to_vec(),
                    mapping: mapping
                        .iter()
                        .map(|(a, b)| (a.to_string(), b.to_string()))
                        .collect(),
                },
            );
        }
        let value_form = serde_json::to_vec(&serde_json::Value::Object(
            source_digest_entries(&sources).unwrap(),
        ))
        .unwrap();
        let streamed = source_digest_entries_bytes(&sources).unwrap();
        assert_eq!(streamed, value_form);
        // spec_digest_identity over a framed object, Value vs streaming.
        let fields = serde_json::json!({
            "profile": "direct-file-template-v1",
            "sources": {"placeholder": true},
            "attributes": {"x": "y"},
        });
        let via_value =
            spec_digest_identity(&fields, &source_digest_entries(&sources).unwrap()).unwrap();
        let mut framed = fields.clone();
        framed.as_object_mut().unwrap().insert(
            "sources".into(),
            serde_json::Value::Object(serde_json::Map::new()),
        );
        // Rebuild the exact streamed framed bytes: serialize `fields`
        // with sources replaced by the streamed entries object.
        let mut manual = Vec::new();
        {
            let obj = fields.as_object().unwrap();
            manual.push(b'{');
            let mut first = true;
            for (key, value) in obj {
                if !first {
                    manual.push(b',');
                }
                first = false;
                manual.extend(serde_json::to_vec(key).unwrap());
                manual.push(b':');
                if key == "sources" {
                    manual.extend_from_slice(&streamed);
                } else {
                    manual.extend(serde_json::to_vec(value).unwrap());
                }
            }
            manual.push(b'}');
        }
        let mut counter = CountWriter(0);
        use std::io::Write as _;
        counter.write_all(&manual).unwrap();
        let mut seeded = SeededDigestWriter::new(SPEC_DIGEST_PROFILE, counter.0);
        seeded.write_all(&manual).unwrap();
        assert_eq!(seeded.finish_hex(), via_value);
        let _ = framed;
    }

    /// The reference spec identity (Step A framing) must equal the
    /// streamed form production uses: byte-identical inner framing, so
    /// pointers published with `spec_digest_identity` keep hitting.
    /// Also pins git-style racy trust: a fingerprint whose times are NOT
    /// at least the racy margin (2s) older than the verify start is never
    /// trusted (hashed instead), and `settled_fingerprint` refuses
    /// fresh files at recording time.
    #[test]
    fn spec_reference_matches_streamed_and_racy_never_trusted() {
        let mut sources = BTreeMap::new();
        sources.insert(
            "//a:x.bzl".to_owned(),
            crate::loading::SourceProvenance {
                bytes: b"one".to_vec(),
                mapping: [("m".to_owned(), "n".to_owned())].into_iter().collect(),
            },
        );
        let fields = serde_json::json!({"profile": "direct-file-template-v1",
            "sources": {"replaced": true}, "attributes": {"x": "y"}});
        let entries = source_digest_entries(&sources).unwrap();
        let reference = spec_digest_identity(&fields, &entries).unwrap();
        let framed_bytes = {
            let obj = fields.as_object().unwrap();
            let mut manual = vec![b'{'];
            let mut first = true;
            for (key, value) in obj {
                if !first {
                    manual.push(b',');
                }
                first = false;
                manual.extend(serde_json::to_vec(key).unwrap());
                manual.push(b':');
                if key == "sources" {
                    manual.extend(source_digest_entries_bytes(&sources).unwrap());
                } else {
                    manual.extend(serde_json::to_vec(value).unwrap());
                }
            }
            manual.push(b'}');
            manual
        };
        let mut counter = CountWriter(0);
        use std::io::Write as _;
        counter.write_all(&framed_bytes).unwrap();
        let mut seeded = SeededDigestWriter::new(SPEC_DIGEST_PROFILE, counter.0);
        seeded.write_all(&framed_bytes).unwrap();
        assert_eq!(
            seeded.finish_hex(),
            reference,
            "streamed != reference spec identity"
        );
        // Racy: fresh fingerprint (mtime/ctime ~= now) must hash, not trust.
        // These steps read the real clock, so they are asserted for an
        // attempt that ran inside the racy window ([`inside_racy_window`]).
        let (racy_now, settled_now, outcome) = inside_racy_window(|| {
            let cache = temp();
            let (tree, _) = publish(cache.path(), b"", write).unwrap();
            let manifest: Manifest =
                serde_json::from_slice(&fs::read(metadata_path(cache.path(), &tree)).unwrap())
                    .unwrap();
            let fresh = fingerprint(&fs::metadata(tree.join("data")).unwrap());
            let racy_now = racy_safe(&fresh, std::time::SystemTime::now());
            let settled_now = settled_fingerprint(
                &fs::metadata(tree.join("data")).unwrap(),
                std::time::SystemTime::now(),
            );
            let mut files = manifest.files.clone();
            files.get_mut("data").unwrap().stat = Some(fresh);
            let outcome = verify_snapshot_detailed(&tree, &files, VerifyMode::Stat).unwrap();
            (fresh, (racy_now, settled_now, outcome))
        });
        assert!(!racy_now);
        assert_eq!(settled_now, None);
        assert!(outcome.matched);
        assert!(
            outcome.hashed > 0,
            "racy fingerprint must fall back to hashing"
        );
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

    /// Execute (the warm probe always fails) and publish with exactly these
    /// recorded probes.
    fn publish_recorded(
        cache: &Path,
        recorded: &'static [u8],
        run: impl FnOnce(&Path) -> Result<Vec<u8>, String>,
    ) -> Result<(PathBuf, Publication), String> {
        materialize(
            cache,
            "test-key",
            b"spec",
            b"",
            |_, _| Ok(false),
            |stage| {
                run(stage).map(|inputs| Executed {
                    inputs,
                    recorded: recorded.to_vec(),
                })
            },
        )
    }

    /// docs/WORKTREES.md: a publication whose recorded probes name a
    /// workspace — it depends on where that workspace is — gets that
    /// workspace's own pointer, never the specification's: each workspace
    /// sharing the store reuses its own, switching between them executes
    /// nothing, and no other workspace (nor a caller without one) follows
    /// it. A publication of no workspace keeps the specification's
    /// pointer, which every workspace follows first.
    #[test]
    fn workspace_dependent_publications_get_a_pointer_per_workspace() {
        let cache = temp();
        let key = spec_identity(b"spec");
        // Like crate::repository_warm::unchanged: probes naming a workspace
        // validate there only.
        let validate = |workspace: &'static str| {
            move |_: &Path, recorded: &[u8]| -> Result<bool, String> {
                Ok(recorded_workspace(recorded).is_none_or(|named| named == workspace))
            }
        };
        let run = |workspace: &'static str, recorded: Option<&'static str>| {
            move |stage: &Path| -> Result<Executed, String> {
                fs::write(stage.join("value.txt"), workspace).map_err(|e| e.to_string())?;
                Ok(Executed {
                    inputs: workspace.as_bytes().to_vec(),
                    recorded: match recorded {
                        Some(named) => format!(r#"{{"version":3,"workspace":"{named}"}}"#),
                        None => r#"{"version":2}"#.to_owned(),
                    }
                    .into_bytes(),
                })
            }
        };
        let materialize_in = |workspace: &'static str, dependent: bool| {
            materialize_lazy_in(
                cache.path(),
                "test-key",
                &key,
                Some(workspace),
                || Ok(b"provenance".to_vec()),
                validate(workspace),
                run(workspace, dependent.then_some(workspace)),
            )
            .unwrap()
        };
        let (a, first) = materialize_in("/ws/a", true);
        let (b, second) = materialize_in("/ws/b", true);
        assert_eq!(
            (first, second),
            (Publication::Executed, Publication::Executed)
        );
        assert_ne!(a, b);
        for _ in 0..2 {
            assert_eq!(
                materialize_in("/ws/a", true),
                (a.clone(), Publication::Reused)
            );
            assert_eq!(
                materialize_in("/ws/b", true),
                (b.clone(), Publication::Reused)
            );
        }
        let specs = cache.path().join("direct/specs");
        assert!(!specs.join(&key).exists());
        for workspace in ["/ws/a", "/ws/b"] {
            let pointer = specs.join(workspace_pointer(&key, workspace));
            assert!(pointer.is_file(), "{}", pointer.display());
        }
        assert_ne!(
            workspace_pointer(&key, "/ws/a"),
            workspace_pointer(&key, "/ws/b")
        );
        assert_eq!(
            try_reuse(cache.path(), "test-key", &key, |_, _| Ok(true)).unwrap(),
            None
        );
        assert_eq!(
            try_reuse_in(
                cache.path(),
                "test-key",
                &key,
                Some("/ws/c"),
                validate("/ws/c")
            )
            .unwrap(),
            None
        );
        // A publication of no workspace: the specification's pointer, which
        // every workspace follows first.
        let (shared, third) = materialize_in("/ws/c", false);
        assert_eq!(third, Publication::Executed);
        assert!(specs.join(&key).is_file());
        for workspace in ["/ws/a", "/ws/c", "/ws/d"] {
            assert_eq!(
                try_reuse_in(
                    cache.path(),
                    "test-key",
                    &key,
                    Some(workspace),
                    validate(workspace)
                )
                .unwrap(),
                Some(shared.clone()),
                "{workspace}"
            );
        }
        // ... unless it is not reusable there: then the workspace's own.
        let refuse_shared = |_: &Path, recorded: &[u8]| -> Result<bool, String> {
            Ok(recorded_workspace(recorded).as_deref() == Some("/ws/b"))
        };
        assert_eq!(
            try_reuse_in(cache.path(), "test-key", &key, Some("/ws/b"), refuse_shared).unwrap(),
            Some(b.clone())
        );
    }

    /// rules_python's python_repository after extracting the interpreter:
    /// `chmod -R ugo-w lib` (no pyc may ever be written into it).
    #[cfg(unix)]
    fn read_only_lib(stage: &Path) -> Result<Vec<u8>, String> {
        use std::os::unix::fs::PermissionsExt;
        let package = stage.join("lib/python3.12/encodings");
        fs::create_dir_all(&package).map_err(|e| e.to_string())?;
        fs::write(package.join("__init__.py"), b"").map_err(|e| e.to_string())?;
        for (path, mode) in [
            (package.join("__init__.py"), 0o444),
            (package, 0o555),
            (stage.join("lib/python3.12"), 0o555),
            (stage.join("lib"), 0o555),
        ] {
            fs::set_permissions(path, fs::Permissions::from_mode(mode))
                .map_err(|e| e.to_string())?;
        }
        Ok(b"exact runtime inputs".to_vec())
    }

    /// The bytes a publication's manifest records as its probes.
    fn recorded_probes(cache: &Path, tree: &Path) -> Vec<u8> {
        parse_manifest(&fs::read(metadata_path(cache, tree)).unwrap())
            .unwrap()
            .recorded
            .unwrap()
    }

    /// Republishing over a tree with read-only directories (a re-execution
    /// whose recorded probes changed keeps the identity but replaces the
    /// tree and its manifest) used to fail the command with `cannot remove
    /// direct quarantine: Permission denied` after the new tree was already
    /// in place, leaving `direct/trees/.trash-*` behind. It now succeeds and
    /// removes the quarantine; an unpublished stage holding read-only
    /// directories is removed as well.
    #[cfg(unix)]
    #[test]
    fn republishing_over_a_read_only_tree_removes_the_quarantine() {
        let cache = temp();
        let (tree, first) =
            publish_recorded(cache.path(), br#"{"version":1}"#, read_only_lib).unwrap();
        assert_eq!(first, Publication::Executed);
        no_stages(cache.path());
        let upgraded = br#"{"version":1,"files":[]}"#;
        let (again, second) = publish_recorded(cache.path(), upgraded, read_only_lib).unwrap();
        assert_eq!(
            (again.as_path(), second),
            (tree.as_path(), Publication::Executed)
        );
        // The quarantine branch ran: the manifest carries the new probes.
        assert_eq!(recorded_probes(cache.path(), &tree), upgraded);
        assert!(tree.join("lib/python3.12/encodings/__init__.py").is_file());
        no_stages(cache.path());
        // Same identity and probes: nothing is republished and the stage,
        // read-only `lib/` included, is removed.
        let (_, third) = publish_recorded(cache.path(), upgraded, read_only_lib).unwrap();
        assert_eq!(third, Publication::Executed);
        no_stages(cache.path());
        crate::repository_ctx::remove_tree(cache.path()).unwrap();
    }

    /// A `.trash-*` an earlier binary could not remove (a read-only tree)
    /// is swept by a quarantining publication, but never while another
    /// publisher holds the quarantine lock (its own quarantine is live
    /// then), and once per process and store: a later leftover waits for
    /// the next process.
    #[cfg(unix)]
    #[test]
    fn stale_quarantines_are_swept_once_when_no_quarantine_is_live() {
        use std::os::unix::fs::PermissionsExt;
        let cache = temp();
        let trees = cache.path().join("direct/trees");
        let leave = |name: &str| {
            let stale = trees.join(name);
            fs::create_dir_all(stale.join("tree")).unwrap();
            read_only_lib(&stale.join("tree")).unwrap();
            fs::set_permissions(stale.join("tree"), fs::Permissions::from_mode(0o555)).unwrap();
            stale
        };
        let stale = leave(".trash-left");
        // Another publisher's quarantine is live: the stale one stays.
        fs::create_dir_all(cache.path().join("direct/locks")).unwrap();
        let live = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(cache.path().join("direct/locks/quarantine"))
            .unwrap();
        FileExt::lock_shared(&live).unwrap();
        let (tree, _) = publish_recorded(cache.path(), br#"{"version":1}"#, read_only_lib).unwrap();
        assert!(stale.is_dir());
        FileExt::unlock(&live).unwrap();
        drop(live);
        publish_recorded(cache.path(), br#"{"version":1,"n":1}"#, read_only_lib).unwrap();
        assert!(!stale.exists());
        // Swept once: the process's later publications list nothing.
        let later = leave(".trash-later");
        publish_recorded(cache.path(), br#"{"version":1,"n":2}"#, read_only_lib).unwrap();
        assert!(later.is_dir());
        assert_eq!(
            recorded_probes(cache.path(), &tree),
            br#"{"version":1,"n":2}"#
        );
        // The next process (its first quarantining publication) sweeps it.
        swept_stores().lock().unwrap().remove(&trees);
        publish_recorded(cache.path(), br#"{"version":1,"n":3}"#, read_only_lib).unwrap();
        assert!(!later.exists());
        no_stages(cache.path());
        crate::repository_ctx::remove_tree(cache.path()).unwrap();
    }

    /// Republishing over an existing tree swaps the stage in with one
    /// exchange (renameat2 RENAME_EXCHANGE): a reader in another process —
    /// an action running through a link into the tree — always finds the
    /// tree's files at their paths. Quarantining the old tree first and
    /// renaming the stage after it left a window without any tree between
    /// the two renames.
    #[cfg(target_os = "linux")]
    #[test]
    fn republication_never_leaves_the_tree_path_without_a_tree() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let cache = temp();
        // Only where the filesystem can exchange (the fallback has the
        // window).
        let (a, b) = (cache.path().join("a"), cache.path().join("b"));
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();
        let (cwd, exchange) = (rustix::fs::CWD, rustix::fs::RenameFlags::EXCHANGE);
        if let Err(error) = rustix::fs::renameat_with(cwd, &a, cwd, &b, exchange) {
            eprintln!("skipped: no RENAME_EXCHANGE in /tmp/opencode ({error})");
            return;
        }
        fs::remove_dir(&a).unwrap();
        fs::remove_dir(&b).unwrap();
        let (tree, _) = publish_recorded(cache.path(), br#"{"version":1}"#, write).unwrap();
        let data = tree.join("data");
        /// Stops the watcher however the test ends (a panic included).
        struct Stop(std::sync::Arc<AtomicBool>);
        impl Drop for Stop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let stop = Stop(std::sync::Arc::new(AtomicBool::new(false)));
        let watcher = {
            let (stop, data) = (std::sync::Arc::clone(&stop.0), data.clone());
            std::thread::spawn(move || {
                let (mut looks, mut missing) = (0_u64, 0_u64);
                while !stop.load(Ordering::Relaxed) {
                    looks += 1;
                    missing += u64::from(fs::symlink_metadata(&data).is_err());
                }
                (looks, missing)
            })
        };
        for round in 1..=100 {
            let recorded = format!(r#"{{"version":1,"n":{round}}}"#);
            let (again, _) = materialize(
                cache.path(),
                "test-key",
                b"spec",
                b"",
                |_, _| Ok(false),
                |stage| {
                    write(stage).map(|inputs| Executed {
                        inputs,
                        recorded: recorded.clone().into_bytes(),
                    })
                },
            )
            .unwrap();
            assert_eq!(again, tree);
        }
        drop(stop);
        let (looks, missing) = watcher.join().unwrap();
        // Every round republished (the manifest carries the last probes).
        assert_eq!(
            recorded_probes(cache.path(), &tree),
            br#"{"version":1,"n":100}"#
        );
        assert_eq!(missing, 0, "{missing} of {looks} looks found no tree");
        assert_eq!(fs::read(&data).unwrap(), [0, 255, 128, 10]);
        no_stages(cache.path());
    }

    fn metadata_path(cache: &Path, tree: &Path) -> PathBuf {
        cache.join("direct/manifests").join(format!(
            "{}.json",
            tree.file_name().unwrap().to_str().unwrap()
        ))
    }

    /// No scratch survives a completed materialization: no `.trash-*` in
    /// the trees, no temporary link among the slots, and every slot is
    /// settled — the link to a published tree (an ordinary repository) or
    /// a real directory with its marker (a self-referencing one).
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
        let Ok(slots) = fs::read_dir(cache.join("external")) else {
            return;
        };
        for slot in slots {
            let slot = slot.unwrap();
            let name = slot.file_name().into_string().unwrap();
            assert!(!name.starts_with('.'), "scratch {name} left in the slots");
            let path = slot.path();
            match fs::read_link(&path) {
                Ok(target) => {
                    let identity = target.strip_prefix("../direct/trees").unwrap();
                    assert!(
                        is_published(&PublishedTree {
                            kind: "direct",
                            id: identity.to_str().unwrap().to_owned(),
                            root: cache.join("direct/trees").join(identity),
                            manifest: cache
                                .join("direct/manifests")
                                .join(format!("{}.json", identity.display())),
                        }),
                        "{name} links {}",
                        target.display()
                    );
                    assert!(path.join("REPO.bazel").exists() || path.join("MODULE.bazel").exists());
                }
                Err(_) => {
                    assert!(fs::symlink_metadata(&path).unwrap().is_dir());
                    assert!(
                        crate::repository_slots::marker(cache, &name).is_some(),
                        "{name} is a real directory without a marker"
                    );
                }
            }
        }
    }

    /// The slot of `key`.
    fn slot(cache: &Path, key: &str) -> PathBuf {
        crate::repository_slots::slot_path(cache, key)
    }

    #[test]
    fn slot_paths_are_deterministic_across_materializations() {
        // Repository content may embed its own working directory (gazelle's
        // go_env writes `ctx.path(".")` into go.env). Every execution of a
        // repository runs in its slot `<cache>/external/<name>`, a pure
        // function of the cache and the name, so re-evaluation reproduces
        // the identity and warm reuse holds — in this process and in any
        // other sharing the cache.
        //
        // The second materialization FORCES re-execution (a failing
        // `validate` falls through to the slot, exactly like a changed
        // watched input): a warm-reusing second call would just follow the
        // spec pointer to the first tree and prove nothing about the slot
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
        // The embedded working directory is the slot, Bazel's
        // <output_base>/external/<name>: <cache>/external/<name>.
        let embedded = fs::read_to_string(first.join("go.env")).unwrap();
        let go_env = slot(cache.path(), "rules_gazelle++go_env");
        assert_eq!(embedded, format!("GOPATH='{}'\n", go_env.display()));
        // It embeds its own directory: the slot stays a real, marked
        // directory and the tree is a copy.
        assert!(fs::symlink_metadata(&go_env).unwrap().is_dir());
        assert_eq!(fs::read_to_string(go_env.join("go.env")).unwrap(), embedded);
        let identity = first.file_name().unwrap().to_str().unwrap().to_owned();
        assert_eq!(
            crate::repository_slots::marker(cache.path(), "rules_gazelle++go_env"),
            Some(identity.clone())
        );
        let manifest =
            parse_manifest(&fs::read(metadata_path(cache.path(), &first)).unwrap()).unwrap();
        assert_eq!(manifest.self_ref, Some(true));
        let (second, publication) = materialize(
            cache.path(),
            "rules_gazelle++go_env",
            b"spec-go-env",
            b"provenance",
            // Invalidate the recorded watchable inputs: the rule must
            // re-execute into the same slot.
            |_, _| Ok(false),
            embed,
        )
        .unwrap();
        assert_eq!(publication, Publication::Executed);
        // Re-executed (not warm-reused), into the same slot, so the embedded
        // path and the content identity both reproduce and no second tree
        // is published.
        assert_eq!(
            fs::read_to_string(second.join("go.env")).unwrap(),
            embedded,
            "re-execution must embed the same slot pathname"
        );
        assert_eq!(first, second);
        assert_eq!(
            fs::read_dir(cache.path().join("direct/trees"))
                .unwrap()
                .count(),
            1
        );
        // A different repository embeds a different slot pathname and gets
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

    /// An ordinary repository's slot becomes its tree (renamed) and links it
    /// afterwards; a warm reuse binds a slot that links elsewhere, is
    /// missing or is an interrupted execution's leftover.
    #[test]
    fn ordinary_slots_link_their_trees_and_reuse_rebinds_them() {
        let cache = temp();
        let (tree, _) = publish_keyed(cache.path(), "ordinary", b"", write).unwrap();
        let link = slot(cache.path(), "ordinary");
        let target =
            crate::repository_slots::link_target(tree.file_name().unwrap().to_str().unwrap());
        assert_eq!(fs::read_link(&link).unwrap(), target);
        assert_eq!(fs::read(link.join("data")).unwrap(), [0, 255, 128, 10]);
        assert_eq!(
            parse_manifest(&fs::read(metadata_path(cache.path(), &tree)).unwrap())
                .unwrap()
                .self_ref,
            None
        );
        for disturb in [
            |link: &Path| {
                fs::remove_file(link).unwrap();
                std::os::unix::fs::symlink("../direct/trees/elsewhere", link).unwrap();
            },
            |link: &Path| fs::remove_file(link).unwrap(),
            |link: &Path| {
                fs::remove_file(link).unwrap();
                fs::create_dir(link).unwrap();
                fs::write(link.join("partial"), b"interrupted").unwrap();
            },
        ] {
            disturb(&link);
            let (again, status) =
                publish_keyed(cache.path(), "ordinary", b"", |_| panic!("must reuse")).unwrap();
            assert_eq!((again, status), (tree.clone(), Publication::Reused));
            assert_eq!(fs::read_link(&link).unwrap(), target);
        }
        no_stages(cache.path());
    }

    #[test]
    fn interrupted_slot_directory_is_replaced_under_its_lock() {
        let cache = temp();
        let stale = slot(cache.path(), "stale-key");
        fs::create_dir_all(&stale).unwrap();
        fs::write(stale.join("leftover"), b"interrupted").unwrap();
        let (tree, _) = publish_keyed(cache.path(), "stale-key", b"", write).unwrap();
        assert_eq!(fs::read(tree.join("data")).unwrap(), [0, 255, 128, 10]);
        assert!(!tree.join("leftover").exists());
        no_stages(cache.path());
    }

    /// A self-referencing repository's tree is a copy: writes into its slot
    /// (another repository's `go` filling GOPATH) never change the tree,
    /// and a warm reuse keeps what was written there; a re-execution
    /// starts the slot afresh and publishes only what the rule wrote.
    #[test]
    fn writes_into_a_self_referencing_slot_never_change_its_tree() {
        let cache = temp();
        let embed = |stage: &Path| {
            fs::write(
                stage.join("go.env"),
                format!("GOPATH={}\n", stage.display()),
            )
            .map_err(|e| e.to_string())?;
            Ok(b"inputs".to_vec())
        };
        let (tree, _) = publish_keyed(cache.path(), "gocache", b"", embed).unwrap();
        let gopath = slot(cache.path(), "gocache");
        fs::create_dir_all(gopath.join("pkg/mod/example.com")).unwrap();
        fs::write(gopath.join("pkg/mod/example.com/m.go"), b"package m\n").unwrap();
        assert!(!tree.join("pkg").exists());
        let (again, status) =
            publish_keyed(cache.path(), "gocache", b"", |_| panic!("must reuse")).unwrap();
        assert_eq!((again, status), (tree.clone(), Publication::Reused));
        assert!(gopath.join("pkg/mod/example.com/m.go").is_file());
        // The tree still verifies: its copy has its own inodes.
        let published = published_tree(cache.path(), &tree).unwrap();
        verify_published_tree(&published).unwrap();
        // Re-executed (the recorded inputs changed): the slot starts empty.
        let (third, status) = materialize(
            cache.path(),
            "gocache",
            b"spec",
            b"",
            |_, _| Ok(false),
            |stage| {
                assert!(!stage.join("pkg").exists());
                embed(stage).map(|inputs| Executed {
                    inputs,
                    recorded: br#"{"version":1}"#.to_vec(),
                })
            },
        )
        .unwrap();
        assert_eq!(
            (third.clone(), status),
            (tree.clone(), Publication::Executed)
        );
        let names: Vec<_> = fs::read_dir(&tree)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        assert_eq!(names, ["REPO.bazel", "go.env"]);
        no_stages(cache.path());
    }

    /// A replay of a local rule runs in its slot and restores it: the link
    /// to the recorded tree (ordinary), the marked mutable directory (a
    /// self-referencing one that reproduced its tree).
    #[test]
    fn replays_run_in_the_slot_and_restore_it() {
        let cache = temp();
        for (key, self_referencing) in [("local-ordinary", false), ("local-self", true)] {
            let run = |stage: &Path| {
                let text = if self_referencing {
                    format!("{}\n", stage.display())
                } else {
                    "constant\n".to_owned()
                };
                fs::write(stage.join("out"), text).map_err(|e| e.to_string())?;
                Ok(Executed {
                    inputs: b"inputs".to_vec(),
                    recorded: Vec::new(),
                })
            };
            let (root, execution) = materialize_local(
                cache.path(),
                key,
                &spec_identity(key.as_bytes()),
                || Ok(Vec::new()),
                run,
            )
            .unwrap();
            let replayed = replay_local(cache.path(), key, &execution, |stage| {
                assert_eq!(stage, slot(cache.path(), key));
                run(stage)
            })
            .unwrap();
            assert_eq!(replayed.snapshot_digest, execution.snapshot_digest);
            assert_eq!(replayed.inputs_digest, execution.inputs_digest);
            let path = slot(cache.path(), key);
            if self_referencing {
                assert!(fs::symlink_metadata(&path).unwrap().is_dir());
                assert_eq!(
                    crate::repository_slots::marker(cache.path(), key),
                    Some(execution.identity.clone())
                );
            } else {
                assert_eq!(
                    fs::read_link(&path).unwrap(),
                    crate::repository_slots::link_target(&execution.identity)
                );
            }
            verify_local_publication(&root, &execution).unwrap();
        }
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
        // The publication is asserted for an attempt that recorded its
        // manifest inside the racy window of every entry (the fingerprint
        // assertion below; [`inside_racy_window`], [`oldest_entry`]).
        let (cache, tree, status) = inside_racy_window(|| {
            let cache = temp();
            let (tree, status) = publish(cache.path(), b"definition", write).unwrap();
            (oldest_entry(&tree), (cache, tree, status))
        });
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
        assert_eq!(
            content_view(&manifest.files),
            content_view(&snapshot(&tree).unwrap())
        );
        // Freshly written files are inside the racy window at publish:
        // they carry NO fingerprint (review R3) and are hash-verified on
        // first warm reuse, which then refreshes them.
        assert!(manifest.files.values().all(|e| e.stat.is_none()));
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

    /// The private stages of binaries before the slots are removed once per
    /// process, except one an earlier binary executes in (its stage lock is
    /// held); the lock files stay.
    #[cfg(unix)]
    #[test]
    fn legacy_stages_are_swept_when_their_locks_are_free() {
        let cache = temp();
        let direct = cache.path().join("direct");
        for slug in ["free", "held", "lockless"] {
            fs::create_dir_all(direct.join("stages").join(slug).join("pkg/mod")).unwrap();
            read_only_lib(&direct.join("stages").join(slug)).unwrap();
        }
        fs::create_dir_all(direct.join("locks")).unwrap();
        for slug in ["free", "held"] {
            fs::write(direct.join("locks").join(format!("stage-{slug}")), b"").unwrap();
        }
        let held = File::open(direct.join("locks/stage-held")).unwrap();
        FileExt::lock_exclusive(&held).unwrap();
        publish(cache.path(), b"", write).unwrap();
        assert!(!direct.join("stages/free").exists());
        assert!(!direct.join("stages/lockless").exists());
        assert!(direct.join("stages/held/pkg/mod").is_dir());
        assert!(direct.join("locks/stage-free").is_file());
        FileExt::unlock(&held).unwrap();
        // Once per process and store: the next publication sweeps nothing.
        publish_spec(cache.path(), b"other", b"", true, write).unwrap();
        assert!(direct.join("stages/held/pkg/mod").is_dir());
        crate::repository_ctx::remove_tree(cache.path()).unwrap();
    }

    /// The profile bump of the slots (v1 -> v2): a pointer an earlier
    /// binary published for the same specification (made in a private
    /// stage, maybe embedding its path) is never reused — the rule runs
    /// once more — while identical content keeps its identity.
    #[test]
    fn earlier_profile_pointers_are_never_reused() {
        let cache = temp();
        let v1 = {
            let mut digest = Sha256::new();
            digest.update(b"direct-repository-spec-v1");
            digest.update([0]);
            digest.update(b"spec");
            format!("{:x}", digest.finalize())
        };
        assert_ne!(v1, spec_identity(b"spec"));
        let (old, _) = materialize_keyed(
            cache.path(),
            "test-key",
            &v1,
            b"",
            |_, _| Ok(true),
            |stage| {
                write(stage).map(|inputs| Executed {
                    inputs,
                    recorded: br#"{"version":1}"#.to_vec(),
                })
            },
        )
        .unwrap();
        let executions = AtomicUsize::new(0);
        let (new, status) = publish(cache.path(), b"", |stage| {
            executions.fetch_add(1, Ordering::SeqCst);
            write(stage)
        })
        .unwrap();
        assert_eq!(
            (status, executions.load(Ordering::SeqCst)),
            (Publication::Executed, 1)
        );
        assert_eq!(new, old);
        assert_ne!(
            super::SPEC_DIGEST_PROFILE,
            b"direct-repository-spec-digest-v1"
        );
        no_stages(cache.path());
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
        // The slot's lock files persist like a publication lock (deleting a
        // lock file that other processes may hold would break exclusion);
        // no slot directory and no tree survive the failure.
        assert!(
            fs::read_dir(cache.path().join("direct/locks"))
                .unwrap()
                .all(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_str()
                    .unwrap()
                    .starts_with("slot-"))
        );
        assert!(!slot(cache.path(), "test-key").exists());
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

    /// Review i20 (forward compatibility): a manifest with fields a newer
    /// binary added — top level, per entry, inside a stat fingerprint — and
    /// a pointer with an extra field are reused, not republished: their
    /// bytes stay untouched, also when changed recorded inputs re-run the
    /// rule with identical output. A manifest of another schema (`format`
    /// or `profile`) is never trusted and is republished as this binary's.
    #[test]
    fn unknown_manifest_fields_are_reused_not_republished() {
        let cache = temp();
        let (tree, _) = publish(cache.path(), b"", write).unwrap();
        let path = metadata_path(cache.path(), &tree);
        let mut newer: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        newer["written_by"] = "a newer bazel-rs".into();
        newer["files"]["data"]["xattrs"] = serde_json::json!({"user.origin": "x"});
        newer["files"]["data"]["stat"] = serde_json::json!(
            {"dev": 1, "ino": 2, "mtime": [3, 4], "ctime": [5, 6], "btime": [7, 8]}
        );
        fs::write(&path, serde_json::to_vec(&newer).unwrap()).unwrap();
        let pointer = cache
            .path()
            .join("direct/specs")
            .join(spec_identity(b"spec"));
        let mut claim: serde_json::Value =
            serde_json::from_slice(&fs::read(&pointer).unwrap()).unwrap();
        claim["written_by"] = "a newer bazel-rs".into();
        fs::write(&pointer, serde_json::to_vec(&claim).unwrap()).unwrap();
        let manifest_bytes = fs::read(&path).unwrap();
        let pointer_bytes = fs::read(&pointer).unwrap();

        let (again, status) = publish(cache.path(), b"", |_| {
            panic!("a newer manifest must be reused")
        })
        .unwrap();
        assert_eq!((again, status), (tree.clone(), Publication::Reused));
        assert_eq!(fs::read(&path).unwrap(), manifest_bytes);
        assert_eq!(fs::read(&pointer).unwrap(), pointer_bytes);
        // Changed recorded inputs re-run the rule; its identical output
        // finds the equivalent newer manifest and leaves it as written.
        let (again, status) = publish_spec(cache.path(), b"spec", b"", false, write).unwrap();
        assert_eq!((again, status), (tree.clone(), Publication::Executed));
        assert_eq!(fs::read(&path).unwrap(), manifest_bytes);
        // A fingerprint refresh never rewrites (so never strips) it either.
        let parsed = parse_manifest(&manifest_bytes).unwrap();
        let settled = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        let outcome = verify_snapshot_at(&tree, &parsed.files, VerifyMode::Stat, settled).unwrap();
        assert!(outcome.matched && outcome.refreshed.contains_key("data"));
        // With the lock provably free, an unchanged manifest below means the
        // refresh declined this manifest, not that it skipped a held lock.
        free_publication_lock(&cache.path().join("direct"), &parsed.identity);
        refresh_manifest(
            &cache.path().join("direct"),
            &parsed.identity,
            &manifest_bytes,
            &outcome.refreshed,
        )
        .unwrap();
        assert_eq!(fs::read(&path).unwrap(), manifest_bytes);

        for (field, value) in [
            ("format", serde_json::json!(2)),
            ("profile", serde_json::json!("direct-file-template-v2")),
        ] {
            let mut other: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();
            other[field] = value;
            fs::write(&path, serde_json::to_vec(&other).unwrap()).unwrap();
            let executions = AtomicUsize::new(0);
            let (again, status) = publish(cache.path(), b"", |stage| {
                executions.fetch_add(1, Ordering::SeqCst);
                write(stage)
            })
            .unwrap();
            assert_eq!((again, status), (tree.clone(), Publication::Executed));
            assert_eq!(executions.load(Ordering::SeqCst), 1, "{field}");
            let republished: Manifest = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            assert_eq!(
                (republished.format, republished.profile.as_str()),
                (MANIFEST_FORMAT, PROFILE)
            );
        }
        no_stages(cache.path());
    }

    /// Review i20: an execution that keeps the publication's identity but
    /// records probes other than the manifest's (an older binary's spelling,
    /// which the warm path no longer trusts) upgrades the manifest once, so
    /// the next warm call reuses instead of re-executing on every run.
    #[test]
    fn changed_recorded_probes_upgrade_the_manifest_once() {
        let cache = temp();
        let (tree, _) = publish(cache.path(), b"", write).unwrap();
        let path = metadata_path(cache.path(), &tree);
        const UPGRADED: &[u8] = br#"{"version":2}"#;
        let run = |execute: bool| {
            materialize(
                cache.path(),
                "test-key",
                b"spec",
                b"",
                |_, recorded| Ok(recorded == UPGRADED),
                |stage| {
                    assert!(execute, "upgraded probes must be reused");
                    write(stage).map(|inputs| Executed {
                        inputs,
                        recorded: UPGRADED.to_vec(),
                    })
                },
            )
            .unwrap()
        };
        assert_eq!(run(true), (tree.clone(), Publication::Executed));
        let manifest: Manifest = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(manifest.recorded.as_deref(), Some(UPGRADED));
        assert_eq!(run(false), (tree.clone(), Publication::Reused));
        no_stages(cache.path());
    }

    /// Review i20: the upgrade is one-directional. This binary cannot
    /// validate a NEWER binary's probes (version 3 here, in a manifest with
    /// a field it does not know), so it executes — but an execution keeping
    /// the identity leaves that publication exactly as written: manifest
    /// bytes, tree and file inodes unchanged, on every run. Replacing it
    /// would strip the newer probes, and the newer binary would re-execute
    /// and republish in turn (the i18.1↔i19 ping-pong). Probes this binary
    /// cannot read at all are kept the same way.
    #[cfg(unix)]
    #[test]
    fn newer_recorded_probes_are_never_replaced() {
        use std::os::unix::fs::MetadataExt;
        let cache = temp();
        let (tree, _) = publish(cache.path(), b"", write).unwrap();
        let path = metadata_path(cache.path(), &tree);
        for newer_probes in [
            &br#"{"version":3,"probes":["a newer spelling"]}"#[..],
            b"\x00a binary probe format",
        ] {
            let mut newer: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            newer["recorded"] = serde_json::to_value(newer_probes).unwrap();
            newer["written_by"] = "a newer bazel-rs".into();
            fs::write(&path, serde_json::to_vec(&newer).unwrap()).unwrap();
            let manifest = fs::read_to_string(&path).unwrap();
            let inodes = || {
                (
                    fs::metadata(&tree).unwrap().ino(),
                    fs::metadata(tree.join("data")).unwrap().ino(),
                )
            };
            let before = inodes();
            let executions = AtomicUsize::new(0);
            for _ in 0..2 {
                let outcome = materialize(
                    cache.path(),
                    "test-key",
                    b"spec",
                    b"",
                    |_, recorded| Ok(recorded_version(recorded) == Some(2)),
                    |stage| {
                        executions.fetch_add(1, Ordering::SeqCst);
                        write(stage).map(|inputs| Executed {
                            inputs,
                            recorded: br#"{"version":2}"#.to_vec(),
                        })
                    },
                )
                .unwrap();
                assert_eq!(outcome, (tree.clone(), Publication::Executed));
                assert_eq!(fs::read_to_string(&path).unwrap(), manifest);
                assert_eq!(inodes(), before);
            }
            assert_eq!(executions.load(Ordering::SeqCst), 2);
        }
        assert!(upgrades_recorded(None, br#"{"version":2}"#));
        assert!(upgrades_recorded(
            Some(br#"{"version":2,"files":[]}"#),
            br#"{"version":2}"#
        ));
        assert!(!upgrades_recorded(
            Some(br#"{"version":2}"#),
            br#"{"version":2}"#
        ));
        assert!(!upgrades_recorded(
            Some(br#"{"version":3}"#),
            br#"{"version":2}"#
        ));
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
            assert_eq!(content_view(&repaired.files), content_view(&expected.files));
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
    /// (restored with futimens, as `touch -d` does) still re-executes —
    /// the ctime moves, the stat fingerprint mismatches, the fallback hash
    /// catches it.
    #[test]
    fn stat_verify_reuse_and_tamper_matrix() {
        let executions = AtomicUsize::new(0);
        let run = |cache: &Path| {
            publish(cache, b"", |stage| {
                executions.fetch_add(1, Ordering::SeqCst);
                write(stage)
            })
            .unwrap()
        };
        // (1) is asserted for an attempt whose publication and warm call
        // both ran inside the racy window of every entry: one stalled past
        // it legitimately records or refreshes settled fingerprints
        // ([`inside_racy_window`], [`oldest_entry`]).
        let (cache, tree, first, (again, status), manifest) = inside_racy_window(|| {
            executions.store(0, Ordering::SeqCst);
            let cache = temp();
            let (tree, first) = run(cache.path());
            // (1) do-nothing: reuse, no execution. Fresh entries carry no
            // fingerprint yet (racy at publish, review R3); a warm verify
            // hash-matches and refreshes only fingerprints settled by then.
            let warm = run(cache.path());
            let manifest: Manifest =
                serde_json::from_slice(&fs::read(metadata_path(cache.path(), &tree)).unwrap())
                    .unwrap();
            (oldest_entry(&tree), (cache, tree, first, warm, manifest))
        });
        assert_eq!(first, Publication::Executed);
        assert_eq!(again, tree);
        assert_eq!(status, Publication::Reused);
        assert_eq!(executions.load(Ordering::SeqCst), 1);
        assert!(manifest.files.values().all(|e| e.stat.is_none()));
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
        File::open(&path).unwrap().set_modified(mtime).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), mtime);
        let (_, status) = run(cache.path());
        assert_eq!(status, Publication::Executed);
        assert_eq!(executions.load(Ordering::SeqCst), 3);
        assert_eq!(fs::read(tree.join("data")).unwrap(), [0, 255, 128, 10]);
        // Full mode agrees on the untouched tree.
        assert!(verify_snapshot(&tree, &snapshot(&tree).unwrap(), VerifyMode::Full).unwrap());
        no_stages(cache.path());
    }

    /// The outcome of the first `attempt` that completed inside the racy
    /// window of the stat fingerprint it returns: before [`RACY_MARGIN`]
    /// had passed since that file's newest time stamp, so every
    /// `SystemTime::now()` the store read during the attempt (snapshot
    /// recording, warm verify start) still found the file unsettled
    /// ([`racy_safe`] is monotonic in its start). A scheduling stall of
    /// RACY_MARGIN or more (a loaded parallel test run) legitimately lets
    /// the file settle mid-attempt; such an attempt proves nothing either
    /// way, so it is repeated instead of asserted on.
    fn inside_racy_window<T>(mut attempt: impl FnMut() -> (StatFingerprint, T)) -> T {
        for _ in 0..10 {
            let (fingerprint, outcome) = attempt();
            if !racy_safe(&fingerprint, std::time::SystemTime::now()) {
                return outcome;
            }
        }
        panic!("ten attempts in a row were stalled past the {RACY_MARGIN:?} racy window");
    }

    /// The newest time stamp (mtime or ctime) of `fingerprint`: its racy
    /// window is `[changed_at, changed_at + RACY_MARGIN)`.
    fn changed_at(fingerprint: &StatFingerprint) -> std::time::SystemTime {
        [fingerprint.mtime, fingerprint.ctime]
            .into_iter()
            .map(|(sec, nsec)| {
                std::time::SystemTime::UNIX_EPOCH
                    + std::time::Duration::new(u64::try_from(sec).unwrap(), nsec as u32)
            })
            .max()
            .unwrap()
    }

    /// The stat fingerprint of the entry below `tree`'s root whose racy
    /// window ends first (the oldest [`changed_at`]). While it is inside
    /// its window, every entry of the tree is: the root's own pre-publish
    /// times are no older (creating an entry updates them), and publishing
    /// renames only the root. An attempt that [`inside_racy_window`]
    /// accepts with this fingerprint therefore recorded its manifest, and
    /// ran any warm verify, before any entry had settled.
    fn oldest_entry(tree: &Path) -> StatFingerprint {
        fn visit(path: &Path, oldest: &mut Option<StatFingerprint>) {
            for entry in fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                let metadata = fs::symlink_metadata(&path).unwrap();
                let candidate = fingerprint(&metadata);
                if oldest.is_none_or(|oldest| changed_at(&candidate) < changed_at(&oldest)) {
                    *oldest = Some(candidate);
                }
                if metadata.is_dir() {
                    visit(&path, oldest);
                }
            }
        }
        let mut oldest = None;
        visit(tree, &mut oldest);
        oldest.expect("a tree with entries")
    }

    /// Wait until no open file description holds the lock of publication
    /// `identity`, and leave it free. Closing a descriptor does not
    /// release its flock while a copy exists: a process start on another
    /// test's thread copies this process's descriptor table, and the
    /// child holds the copies until its exec closes them (O_CLOEXEC; the
    /// window src/spawn_lock.rs describes for write descriptors). So a
    /// lock this test (or a `refresh_manifest` it called) held and closed
    /// can stay held for a while, and a try-locked refresh would skip.
    /// Blocking waits such a copy out; the explicit unlock (it acts on
    /// the shared open file description) leaves no copy of THIS
    /// descriptor holding it.
    fn free_publication_lock(direct: &Path, identity: &str) {
        let lock = File::open(direct.join("locks").join(identity)).unwrap();
        FileExt::lock_exclusive(&lock).unwrap();
        FileExt::unlock(&lock).unwrap();
    }

    /// Review R3: fingerprints settle through use. A freshly published
    /// manifest carries none (racy at publish); the first warm verify
    /// hashes + refreshes once the file is settled; a later external
    /// ctime bump (chmod) hash-matches once and refreshes again instead
    /// of re-hashing forever; a settled fingerprint then trusts by stat.
    ///
    /// Deterministic under the parallel test runner: the steps that read
    /// the real clock are asserted for an attempt that provably ran
    /// inside the racy window ([`inside_racy_window`]), the other racy
    /// decisions take explicit reference instants, and the refresh runs
    /// with the publication lock provably free ([`free_publication_lock`]).
    #[test]
    fn fingerprint_refresh_settles_after_hash_match() {
        let (cache, tree, published) = inside_racy_window(|| {
            let cache = temp();
            let (tree, _) = publish(cache.path(), b"", write).unwrap();
            let manifest = fs::read(metadata_path(cache.path(), &tree)).unwrap();
            let data = fingerprint(&fs::metadata(tree.join("data")).unwrap());
            (data, (cache, tree, parse_manifest(&manifest).unwrap()))
        });
        let read_manifest = |cache: &Path| -> Manifest {
            serde_json::from_slice(&fs::read(metadata_path(cache, &tree)).unwrap()).unwrap()
        };
        assert!(published.files["data"].stat.is_none());
        let (status, data_stat) = inside_racy_window(|| {
            // Backdate `data`'s mtime an hour (futimens; the ctime becomes
            // now): content/state guards (mode, size, hash) are
            // unaffected; only the fingerprint logic reacts.
            let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
            File::open(tree.join("data"))
                .unwrap()
                .set_modified(old)
                .unwrap();
            // Drop persisted fingerprints (legacy-manifest state).
            let path = metadata_path(cache.path(), &tree);
            let mut manifest: Manifest = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            for entry in manifest.files.values_mut() {
                entry.stat = None;
            }
            fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
            // Warm verify through the real path: fresh ctime means the
            // fingerprint is NOT settled relative to the verify start, so
            // this reuse hash-matches WITHOUT refreshing yet.
            let (_, status) = publish(cache.path(), b"", |_| panic!("must reuse")).unwrap();
            let data = fingerprint(&fs::metadata(tree.join("data")).unwrap());
            (
                data,
                (status, read_manifest(cache.path()).files["data"].stat),
            )
        });
        assert_eq!(status, Publication::Reused);
        assert!(data_stat.is_none());
        // Settlement, deterministically: verify as of +1h. The file's
        // times are settled relative to that start, so the hash match
        // REFRESHES the missing fingerprint with the current stat.
        let manifest = read_manifest(cache.path());
        let settled_start = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        let outcome =
            verify_snapshot_at(&tree, &manifest.files, VerifyMode::Stat, settled_start).unwrap();
        assert!(outcome.matched);
        let current = fingerprint(&fs::metadata(tree.join("data")).unwrap());
        assert_eq!(outcome.refreshed.get("data"), Some(&current));
        // A start inside the racy window of the backdating's ctime (as a
        // verify right after it) neither trusts nor records the stat.
        let racy_start = changed_at(&current) + RACY_MARGIN / 2;
        assert!(!racy_safe(&current, racy_start));
        assert!(racy_safe(&current, settled_start));
        // Recording-side gate (review R3): a fresh file gets no
        // fingerprint at publish; the same stat is settled vs +1h.
        let metadata = fs::metadata(tree.join("data")).unwrap();
        assert_eq!(settled_fingerprint(&metadata, racy_start), None);
        assert_eq!(settled_fingerprint(&metadata, settled_start), Some(current));
        // Persist through refresh_manifest and observe the manifest.
        free_publication_lock(&cache.path().join("direct"), &manifest.identity);
        refresh_manifest(
            &cache.path().join("direct"),
            &manifest.identity,
            &fs::read(metadata_path(cache.path(), &tree)).unwrap(),
            &outcome.refreshed,
        )
        .unwrap();
        let manifest = read_manifest(cache.path());
        assert_eq!(manifest.files["data"].stat, Some(current));
        // Fingerprint matches but is fresh (racy) vs now: verify still
        // hashes (no refresh, no trust) — and still matches.
        let outcome = verify_snapshot_detailed(&tree, &manifest.files, VerifyMode::Stat).unwrap();
        assert!(outcome.matched);
        assert!(outcome.refreshed.is_empty());
        // Settled vs +1h with the fingerprint recorded: trusted by stat,
        // zero files hashed.
        let outcome =
            verify_snapshot_at(&tree, &manifest.files, VerifyMode::Stat, settled_start).unwrap();
        assert!(outcome.matched);
        assert!(outcome.refreshed.is_empty());
        assert_eq!(
            outcome.hashed, 0,
            "settled fingerprint must trust by stat without hashing"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // External ctime bump only: round-trip the mode back to the
            // recorded 0o755 — content mode unchanged, ctime bumped. File
            // times come from a coarse clock (one tick, ~1 ms here): the
            // round trip repeats until the ctime provably moved.
            for attempt in 0.. {
                fs::set_permissions(tree.join("data"), fs::Permissions::from_mode(0o700)).unwrap();
                fs::set_permissions(tree.join("data"), fs::Permissions::from_mode(0o755)).unwrap();
                if fingerprint(&fs::metadata(tree.join("data")).unwrap()) != current {
                    break;
                }
                assert!(attempt < 5000, "the ctime never moved");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let manifest = read_manifest(cache.path());
            let outcome =
                verify_snapshot_detailed(&tree, &manifest.files, VerifyMode::Stat).unwrap();
            assert!(outcome.matched);
            // The bumped file hashed (fingerprint stale) — the counter
            // proves the fallback ran; refresh is empty only because the
            // bumped stat is still fresh (correct: it settles later).
            assert!(outcome.hashed > 0, "ctime bump must fall back to hashing");
        }
        no_stages(cache.path());
    }

    /// Review i20: concurrent warm probes (worker threads here; processes
    /// contend on the same flock) refresh the manifest safely: every probe
    /// reuses, the manifest ends with the settled fingerprint and intact
    /// content in the canonical bytes, no staged refresh is left behind,
    /// and a later probe rewrites nothing (idempotent).
    #[test]
    fn concurrent_warm_probes_refresh_the_manifest_safely() {
        // Racy at publish: asserted for a publication recorded inside the
        // racy window of every entry ([`inside_racy_window`]).
        let (cache, tree, path, published) = inside_racy_window(|| {
            let cache = temp();
            let (tree, _) = publish(cache.path(), b"", write).unwrap();
            let path = metadata_path(cache.path(), &tree);
            let published = parse_manifest(&fs::read(&path).unwrap()).unwrap();
            (oldest_entry(&tree), (cache, tree, path, published))
        });
        assert!(published.files["data"].stat.is_none()); // racy at publish
        // Let the tree settle past the racy margin, so the probes' fallback
        // hash matches refresh the fingerprint for real.
        std::thread::sleep(RACY_MARGIN + std::time::Duration::from_millis(200));
        // The probes only TRY the publication lock; a copy of the
        // publisher's descriptor in a child another test is starting would
        // make every one of them skip ([`free_publication_lock`]).
        free_publication_lock(&cache.path().join("direct"), &published.identity);
        let spec_key = spec_identity(b"spec");
        let barrier = Arc::new(Barrier::new(16));
        let probes: Vec<_> = (0..16)
            .map(|_| {
                let cache = cache.path().to_owned();
                let spec_key = spec_key.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    try_reuse(&cache, "test-key", &spec_key, |_, _| Ok(true))
                })
            })
            .collect();
        for probe in probes {
            assert_eq!(probe.join().unwrap().unwrap(), Some(tree.clone()));
        }
        let bytes = fs::read(&path).unwrap();
        let refreshed = parse_manifest(&bytes).unwrap();
        assert_eq!(serde_json::to_vec(&refreshed).unwrap(), bytes);
        assert_eq!(
            (&refreshed.identity, &refreshed.spec, &refreshed.recorded),
            (&published.identity, &published.spec, &published.recorded)
        );
        assert_eq!(
            content_view(&refreshed.files),
            content_view(&published.files)
        );
        assert_eq!(
            refreshed.files["data"].stat,
            Some(fingerprint(&fs::metadata(tree.join("data")).unwrap()))
        );
        let manifests: Vec<_> = fs::read_dir(cache.path().join("direct/manifests"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(manifests, [path.file_name().unwrap()]);
        assert_eq!(
            try_reuse(cache.path(), "test-key", &spec_key, |_, _| Ok(true)).unwrap(),
            Some(tree.clone())
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
        no_stages(cache.path());
    }

    /// A warm probe never misses a valid publication because another probe
    /// renames a refreshed manifest over the one it is reading (here a
    /// tight loop renaming identical bytes into place): it reads the opened
    /// file, which a rename leaves complete ([`read_published`]). Before:
    /// 301 of 2,000 probes missed ("changed while reading"), and
    /// `concurrent_warm_probes_refresh_the_manifest_safely` failed 7 of 300
    /// loaded runs on "every probe reuses".
    #[test]
    fn probes_read_manifests_that_are_renamed_over_meanwhile() {
        let cache = temp();
        let (tree, _) = publish(cache.path(), b"", write).unwrap();
        let path = metadata_path(cache.path(), &tree);
        let bytes = fs::read(&path).unwrap();
        let spec_key = spec_identity(b"spec");
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let renamer = {
            let (stop, path, bytes) = (stop.clone(), path.clone(), bytes.clone());
            std::thread::spawn(move || {
                let mut renames = 0u64;
                while !stop.load(Ordering::SeqCst) {
                    let mut stage =
                        tempfile::NamedTempFile::new_in(path.parent().unwrap()).unwrap();
                    stage.write_all(&bytes).unwrap();
                    stage.persist(&path).unwrap();
                    renames += 1;
                }
                renames
            })
        };
        let misses = (0..2000)
            .filter(|_| {
                try_reuse(cache.path(), "test-key", &spec_key, |_, _| Ok(true))
                    .unwrap()
                    .is_none()
            })
            .count();
        stop.store(true, Ordering::SeqCst);
        let renames = renamer.join().unwrap();
        assert!(renames > 0);
        assert_eq!(
            misses, 0,
            "{misses} of 2,000 probes missed ({renames} renames)"
        );
    }

    /// A republication renames the old manifest away before it renames the
    /// new one in: a probe that finds no manifest under a valid pointer
    /// looks again after a pause (bounded), so the publication in between
    /// is not missed. A missing pointer (a specification never published)
    /// is never waited for, and a manifest that stays missing is given up
    /// after [`PUBLISHED_READ_ATTEMPTS`] attempts.
    #[test]
    fn probes_wait_briefly_for_a_manifest_between_two_renames() {
        let cache = temp();
        let (tree, _) = publish(cache.path(), b"", write).unwrap();
        let path = metadata_path(cache.path(), &tree);
        let away = cache.path().join("direct/manifests/away");
        let spec_key = spec_identity(b"spec");
        let retries = Arc::new(std::sync::Mutex::new(Vec::new()));
        let hook = |restore: bool| {
            let (retries, path, away) = (retries.clone(), path.clone(), away.clone());
            BEFORE_PUBLISHED_RETRY.with(|cell| {
                *cell.borrow_mut() = Some(Box::new(move |attempt| {
                    retries.lock().unwrap().push(attempt);
                    if restore && attempt == 2 {
                        fs::rename(&away, &path).unwrap();
                    }
                }));
            });
        };
        // The new manifest arrives during the probe's retries.
        fs::rename(&path, &away).unwrap();
        hook(true);
        assert_eq!(
            try_reuse(cache.path(), "test-key", &spec_key, |_, _| Ok(true)).unwrap(),
            Some(tree.clone())
        );
        assert_eq!(*retries.lock().unwrap(), [1, 2]);
        // A specification never published: no waiting.
        retries.lock().unwrap().clear();
        assert_eq!(
            try_reuse(
                cache.path(),
                "test-key",
                &spec_identity(b"other"),
                |_, _| Ok(true)
            )
            .unwrap(),
            None
        );
        assert!(retries.lock().unwrap().is_empty());
        // A manifest that stays missing: given up after the last attempt.
        fs::rename(&path, &away).unwrap();
        hook(false);
        assert_eq!(
            try_reuse(cache.path(), "test-key", &spec_key, |_, _| Ok(true)).unwrap(),
            None
        );
        assert_eq!(
            *retries.lock().unwrap(),
            (1..PUBLISHED_READ_ATTEMPTS).collect::<Vec<_>>()
        );
        BEFORE_PUBLISHED_RETRY.with(|cell| cell.borrow_mut().take());
    }

    /// Review i20: the refresh is a compare-and-swap under the publication
    /// lock — skipped (never awaited: this thread holds the lock) while a
    /// publisher holds it, skipped when the manifest changed since the
    /// probe read it (the newer manifest wins), applied once otherwise.
    /// Every call after the publisher's is made with the lock provably free
    /// ([`free_publication_lock`]), so each outcome has exactly the cause
    /// asserted.
    #[test]
    fn manifest_refresh_is_a_locked_compare_and_swap() {
        let cache = temp();
        let (tree, _) = publish(cache.path(), b"", write).unwrap();
        let direct = cache.path().join("direct");
        let path = metadata_path(cache.path(), &tree);
        // The probe's read, explicitly without fingerprints: what a publish
        // records for files younger than the racy margin (one stalled past
        // the margin would record settled ones, which need no refresh).
        let mut manifest = parse_manifest(&fs::read(&path).unwrap()).unwrap();
        for entry in manifest.files.values_mut() {
            entry.stat = None;
        }
        let original = serde_json::to_vec(&manifest).unwrap();
        fs::write(&path, &original).unwrap();
        let identity = manifest.identity.clone();
        let settled = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        let outcome =
            verify_snapshot_at(&tree, &manifest.files, VerifyMode::Stat, settled).unwrap();
        let refreshed = outcome.refreshed["data"];
        {
            let publisher = File::open(direct.join("locks").join(&identity)).unwrap();
            FileExt::lock_exclusive(&publisher).unwrap();
            refresh_manifest(&direct, &identity, &original, &outcome.refreshed).unwrap();
            assert_eq!(fs::read(&path).unwrap(), original);
            // Released explicitly: a copy of this descriptor in a child
            // another test is starting would keep a closed one locked.
            FileExt::unlock(&publisher).unwrap();
        }
        let mut other = manifest.clone();
        other.files.get_mut("data").unwrap().stat = Some(StatFingerprint {
            dev: 1,
            ino: 2,
            mtime: (3, 4),
            ctime: (5, 6),
        });
        let newer = serde_json::to_vec(&other).unwrap();
        fs::write(&path, &newer).unwrap();
        free_publication_lock(&direct, &identity);
        refresh_manifest(&direct, &identity, &original, &outcome.refreshed).unwrap();
        assert_eq!(fs::read(&path).unwrap(), newer);
        fs::write(&path, &original).unwrap();
        free_publication_lock(&direct, &identity);
        refresh_manifest(&direct, &identity, &original, &outcome.refreshed).unwrap();
        let written = fs::read(&path).unwrap();
        assert_eq!(
            parse_manifest(&written).unwrap().files["data"].stat,
            Some(refreshed)
        );
        free_publication_lock(&direct, &identity);
        refresh_manifest(&direct, &identity, &written, &outcome.refreshed).unwrap();
        assert_eq!(fs::read(&path).unwrap(), written);
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
