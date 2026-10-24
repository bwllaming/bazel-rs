//! The binary encoding of a [`PlannedRequest`] (L2-W4, PLAN-L2 §1d): postcard
//! (serde, varint integers) in a small sectioned container, plus the serde
//! field helpers the planned types use, and the full-analysis path's
//! hand-over ([`hand_over`]) that round-trips a request through the codec
//! under `BAZEL_RS_SNAPSHOT_ROUNDTRIP=1`.
//!
//! Container (all integers little-endian):
//!
//! | offset | content |
//! |---|---|
//! | 0 | magic `BZRSPLN1` |
//! | 8 | format version (u32) |
//! | 12 | section count (u32) |
//! | 16 | section table: per section its kind (u32) and length (u64) |
//! | … | the sections in table order: one `META` (everything but the actions), then the actions in `NODES` chunks of at most [`CHUNK`] actions |
//!
//! The section framing is decoded without copying (borrowed slices), so a
//! reader can inspect the META section without touching the actions, and
//! large plans decode their chunks on several threads. The planned types
//! themselves are owned (they move into the scheduler); paths and byte
//! strings are encoded as raw bytes (serde's `PathBuf` refuses non-UTF-8
//! paths, and lone bytes exist in this codebase, crate::bazel_bytes).
//! Snapshots never survive a binary change (the store keys them by the
//! binary's identity), so the derives sit directly on the in-memory types:
//! no mirror types, no migrations.
//!
//! What the encoding leaves out, by design: an action's resolved client
//! environment values (re-resolved by crate::request finalize on every run,
//! and never written to disk), the workspace status (placeholders, expanded
//! by finalize) and an Args `map_each` function (an in-memory Starlark
//! value: a request that still needs one cannot be encoded,
//! [`EncodeError::NotSerializable`]).

use crate::request::PlannedRequest;
use crate::scheduler::PlannedAction;
use serde::de::{Deserializer, Visitor};
use serde::ser::{Error as _, Serializer};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The container magic.
pub const MAGIC: &[u8; 8] = b"BZRSPLN1";
/// The container format version (bumped with any layout change; the store
/// also keys snapshots by the binary's identity).
pub const FORMAT: u32 = 1;
/// Actions per NODES section (fixed: the encoding must not depend on the
/// machine).
pub const CHUNK: usize = 1024;
/// Below this many encoded action bytes, chunks decode on the calling thread.
const PARALLEL_DECODE_BYTES: usize = 4 << 20;
/// At most this many threads encode or decode the chunks of one plan (a
/// `query //src/...`-sized request has hundreds of chunks; the hit path
/// decodes beside the validation and replay threads).
pub const MAX_THREADS: usize = 8;

const SECTION_META: u32 = 1;
const SECTION_NODES: u32 = 2;

/// Why a request cannot be encoded.
#[derive(Debug)]
pub enum EncodeError {
    /// The request holds an in-memory value with no encoding (an Args
    /// `map_each` of a surviving action, crate::command_line::DeferredVector).
    NotSerializable(String),
}

impl std::fmt::Display for EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotSerializable(reason) => write!(f, "not serializable: {reason}"),
        }
    }
}

/// The sizes and times of one encoding (the record mode's report, PERF-PLAN).
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub nodes: usize,
    pub bytes: usize,
    pub meta_bytes: usize,
    pub chunks: usize,
    pub encode: Duration,
    pub decode: Option<Duration>,
}

/// Everything but the actions, borrowed for encoding; field order is the
/// order of [`MetaOwned`].
#[derive(Serialize)]
struct Meta<'a> {
    command: &'a crate::request::RequestCommand,
    execute: bool,
    requested: &'a [(crate::label::Label, crate::request::Produced)],
    aspect_reports: &'a [(String, crate::label::Label, Vec<crate::request::Produced>)],
    non_empty_targets: usize,
    run_plans: &'a [(crate::label::Label, crate::request::PlannedRun)],
    configurations: &'a [crate::request::ConfigurationSummary],
    warnings: &'a [String],
    errors: &'a [String],
    build_info: &'a [String],
}

#[derive(Deserialize)]
struct MetaOwned {
    command: crate::request::RequestCommand,
    execute: bool,
    requested: Vec<(crate::label::Label, crate::request::Produced)>,
    aspect_reports: Vec<(String, crate::label::Label, Vec<crate::request::Produced>)>,
    non_empty_targets: usize,
    run_plans: Vec<(crate::label::Label, crate::request::PlannedRun)>,
    configurations: Vec<crate::request::ConfigurationSummary>,
    warnings: Vec<String>,
    errors: Vec<String>,
    build_info: Vec<String>,
}

fn postcard_error(error: postcard::Error, what: &str) -> EncodeError {
    EncodeError::NotSerializable(format!("{what}: {error}"))
}

/// Encode `request` (deterministic: the same request gives the same bytes).
pub fn encode(request: &PlannedRequest) -> Result<Vec<u8>, EncodeError> {
    encode_with_stats(request).map(|(bytes, _)| bytes)
}

/// The META section's content of `request`.
fn meta_of(request: &PlannedRequest) -> Meta<'_> {
    // Every field by name (no `..`): a new field of PlannedRequest is a
    // compile error here until the encoding carries it (META, or the NODES
    // chunks like `nodes`).
    let PlannedRequest {
        command,
        execute,
        nodes: _,
        requested,
        aspect_reports,
        non_empty_targets,
        run_plans,
        configurations,
        warnings,
        errors,
        build_info,
    } = request;
    Meta {
        command,
        execute: *execute,
        requested,
        aspect_reports,
        non_empty_targets: *non_empty_targets,
        run_plans,
        configurations,
        warnings,
        errors,
        build_info,
    }
}

/// [`encode`], with its sizes and time.
pub fn encode_with_stats(request: &PlannedRequest) -> Result<(Vec<u8>, Stats), EncodeError> {
    let start = Instant::now();
    let meta = postcard::to_stdvec(&meta_of(request))
        .map_err(|error| postcard_error(error, "the request outline"))?;
    let encode_chunk = |chunk: &[PlannedAction]| {
        postcard::to_stdvec(chunk)
            .map_err(|error| EncodeError::NotSerializable(not_serializable_reason(chunk, error)))
    };
    let parts: Vec<&[PlannedAction]> = request.nodes.chunks(CHUNK).collect();
    // Chunks encode independently (the same bytes on any thread): a large
    // plan encodes them on a few scoped threads (the analysis snapshot's
    // save, L2-W5, encodes on the main thread before execution).
    let chunks = on_threads(&parts, |chunk| encode_chunk(chunk))
        .into_iter()
        .map(|chunk| {
            chunk.unwrap_or_else(|| {
                Err(EncodeError::NotSerializable(
                    "the planned actions: the encoder panicked".to_owned(),
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let sections = 1 + chunks.len();
    let mut bytes = Vec::with_capacity(
        16 + 12 * sections + meta.len() + chunks.iter().map(Vec::len).sum::<usize>(),
    );
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&FORMAT.to_le_bytes());
    bytes.extend_from_slice(&(sections as u32).to_le_bytes());
    bytes.extend_from_slice(&SECTION_META.to_le_bytes());
    bytes.extend_from_slice(&(meta.len() as u64).to_le_bytes());
    for chunk in &chunks {
        bytes.extend_from_slice(&SECTION_NODES.to_le_bytes());
        bytes.extend_from_slice(&(chunk.len() as u64).to_le_bytes());
    }
    bytes.extend_from_slice(&meta);
    for chunk in &chunks {
        bytes.extend_from_slice(chunk);
    }
    let stats = Stats {
        nodes: request.nodes.len(),
        bytes: bytes.len(),
        meta_bytes: meta.len(),
        chunks: chunks.len(),
        encode: start.elapsed(),
        decode: None,
    };
    Ok((bytes, stats))
}

/// The first action of `chunk` that cannot be encoded and why, for the
/// error text (postcard's own error does not carry serde's message).
fn not_serializable_reason(chunk: &[PlannedAction], error: postcard::Error) -> String {
    let Some(node) = chunk.iter().find(|node| postcard::to_stdvec(node).is_err()) else {
        return format!("the planned actions: {error}");
    };
    let blobs = embedded_blobs();
    let reason = if node
        .deferred
        .as_ref()
        .is_some_and(|deferred| deferred.uses_map_each())
    {
        "an Args map_each function expands a tree artifact at execution time".to_owned()
    } else if let Some((destination, _)) = node
        .action
        .embedded_inputs
        .iter()
        .find(|(_, bytes)| !blobs.contains(bytes))
    {
        format!(
            "embedded input {} is not a compiled-in blob",
            destination.display()
        )
    } else {
        error.to_string()
    };
    format!("action '{}': {reason}", node.action.id)
}

/// Whether `request` encodes to exactly `bytes`, checked one section at a
/// time (the round trip's check, without a second copy of the encoding).
fn encodes_to(request: &PlannedRequest, bytes: &[u8]) -> Result<bool, String> {
    fn encoded<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, String> {
        postcard::to_stdvec(value).map_err(|error| error.to_string())
    }
    let sections = sections(bytes)?;
    if encoded(&meta_of(request))? != sections.meta {
        return Ok(false);
    }
    let chunks: Vec<&[PlannedAction]> = request.nodes.chunks(CHUNK).collect();
    if chunks.len() != sections.nodes.len() {
        return Ok(false);
    }
    for (chunk, section) in chunks.into_iter().zip(sections.nodes) {
        if encoded(chunk)? != section {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The container's sections, borrowed: the META section and the NODES chunks.
pub struct Sections<'a> {
    pub meta: &'a [u8],
    pub nodes: Vec<&'a [u8]>,
}

fn take<'a>(bytes: &mut &'a [u8], count: usize, what: &str) -> Result<&'a [u8], String> {
    if bytes.len() < count {
        return Err(format!("truncated planned request ({what})"));
    }
    let (head, rest) = bytes.split_at(count);
    *bytes = rest;
    Ok(head)
}

fn u32_at(bytes: &mut &[u8], what: &str) -> Result<u32, String> {
    let head = take(bytes, 4, what)?;
    Ok(u32::from_le_bytes(head.try_into().expect("four bytes")))
}

fn u64_at(bytes: &mut &[u8], what: &str) -> Result<u64, String> {
    let head = take(bytes, 8, what)?;
    Ok(u64::from_le_bytes(head.try_into().expect("eight bytes")))
}

/// Split an encoded request into its sections (no copying, no decoding).
pub fn sections(bytes: &[u8]) -> Result<Sections<'_>, String> {
    let mut rest = bytes;
    if take(&mut rest, 8, "magic")? != MAGIC {
        return Err("not a planned request (bad magic)".to_owned());
    }
    let format = u32_at(&mut rest, "format")?;
    if format != FORMAT {
        return Err(format!(
            "planned request format {format}, this binary reads {FORMAT}"
        ));
    }
    let count = u32_at(&mut rest, "section count")? as usize;
    let mut table = Vec::with_capacity(count.min(1 << 16));
    for _ in 0..count {
        let kind = u32_at(&mut rest, "section table")?;
        let length = usize::try_from(u64_at(&mut rest, "section table")?)
            .map_err(|_| "section length overflows".to_owned())?;
        table.push((kind, length));
    }
    let mut meta = None;
    let mut nodes = Vec::new();
    for (kind, length) in table {
        let body = take(&mut rest, length, "section")?;
        match kind {
            SECTION_META if meta.is_none() => meta = Some(body),
            SECTION_NODES => nodes.push(body),
            _ => return Err(format!("unexpected section kind {kind}")),
        }
    }
    if !rest.is_empty() {
        return Err("trailing bytes after the last section".to_owned());
    }
    Ok(Sections {
        meta: meta.ok_or_else(|| "missing META section".to_owned())?,
        nodes,
    })
}

fn decode_chunk(chunk: &[u8]) -> Result<Vec<PlannedAction>, String> {
    postcard::from_bytes(chunk).map_err(|error| format!("planned actions: {error}"))
}

/// `work` over `items`, results in item order, on min(items, available
/// parallelism, [`MAX_THREADS`]) scoped threads that take the next item from
/// a shared counter (on the calling thread for one item or one thread).
/// `None` for an item whose work panicked.
fn on_threads<T: Sync, R: Send>(items: &[T], work: impl Fn(&T) -> R + Sync) -> Vec<Option<R>> {
    let threads = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(MAX_THREADS)
        .min(items.len());
    let run = |item: &T| std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(item))).ok();
    if threads <= 1 {
        return items.iter().map(run).collect();
    }
    let next = AtomicUsize::new(0);
    let slots: Vec<Mutex<Option<R>>> = items.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(index) else {
                        break;
                    };
                    let result = run(item);
                    *slots[index].lock().unwrap_or_else(PoisonError::into_inner) = result;
                }
            });
        }
    });
    slots
        .into_iter()
        .map(|slot| slot.into_inner().unwrap_or_else(PoisonError::into_inner))
        .collect()
}

/// Decode an encoded request.
pub fn decode(bytes: &[u8]) -> Result<PlannedRequest, String> {
    let sections = sections(bytes)?;
    let meta: MetaOwned = postcard::from_bytes(sections.meta)
        .map_err(|error| format!("planned request outline: {error}"))?;
    let total: usize = sections.nodes.iter().map(|chunk| chunk.len()).sum();
    let chunks: Vec<Result<Vec<PlannedAction>, String>> = if sections.nodes.len() > 1
        && total >= PARALLEL_DECODE_BYTES
    {
        on_threads(&sections.nodes, |chunk| decode_chunk(chunk))
            .into_iter()
            .map(|chunk| chunk.unwrap_or_else(|| Err("planned actions: decoder panicked".into())))
            .collect()
    } else {
        sections
            .nodes
            .iter()
            .map(|chunk| decode_chunk(chunk))
            .collect()
    };
    let mut nodes = Vec::with_capacity(chunks.iter().map(|c| c.as_ref().map_or(0, Vec::len)).sum());
    for chunk in chunks {
        nodes.extend(chunk?);
    }
    Ok(PlannedRequest {
        command: meta.command,
        execute: meta.execute,
        nodes,
        requested: meta.requested,
        aspect_reports: meta.aspect_reports,
        non_empty_targets: meta.non_empty_targets,
        run_plans: meta.run_plans,
        configurations: meta.configurations,
        warnings: meta.warnings,
        errors: meta.errors,
        build_info: meta.build_info,
    })
}

// ---- the full-analysis hand-over -------------------------------------------

/// Round-trip every full-analysis request through the codec and execute the
/// DECODED one (`1`); the CI and A/B switch that proves the encoding loses
/// nothing execution and reporting read.
pub const ROUNDTRIP_VAR: &str = "BAZEL_RS_SNAPSHOT_ROUNDTRIP";
/// Write the encoded request to this file (the determinism gate compares
/// it across thread counts and loading-order jitter seeds).
pub const PLAN_OUT_VAR: &str = "BAZEL_RS_SNAPSHOT_PLAN_OUT";

/// The full-analysis path's hand-over of a planned request to execution
/// (crate::request): with [`ROUNDTRIP_VAR`]`=1` the request is encoded,
/// decoded and re-encoded (the bytes must be identical) and the decoded
/// request is returned; with [`PLAN_OUT_VAR`] the encoding is written to
/// that file; in the record mode (`BAZEL_RS_SNAPSHOT=record`) the encoding
/// is measured for its report. Without any of them nothing is encoded.
///
/// A request that cannot be encoded (a surviving `map_each`) is returned
/// as it is; the record mode reports why. A decoding or re-encoding
/// mismatch is an internal error of the codec.
pub fn hand_over(request: PlannedRequest) -> Result<PlannedRequest, String> {
    let roundtrip = std::env::var_os(ROUNDTRIP_VAR).is_some_and(|value| value == "1");
    let plan_out = std::env::var_os(PLAN_OUT_VAR).map(PathBuf::from);
    let record = crate::snapshot::record::active();
    if !roundtrip && plan_out.is_none() && !record {
        return Ok(request);
    }
    let (bytes, mut stats) = match encode_with_stats(&request) {
        Ok(encoded) => encoded,
        Err(error) => {
            crate::snapshot::record::note_plan(Err(error.to_string()));
            perf_line(&format!("not encoded ({error})"));
            return Ok(request);
        }
    };
    if let Some(path) = &plan_out
        && let Err(error) = std::fs::write(path, &bytes)
    {
        return Err(format!(
            "{PLAN_OUT_VAR}={}: {error}",
            path.to_string_lossy()
        ));
    }
    let result = if roundtrip {
        // The decoded request executes: the planned one is not needed any
        // more (one copy of the plan in memory, as on a hit).
        drop(request);
        let start = Instant::now();
        let decoded = decode(&bytes).map_err(|error| {
            format!("internal error: the planned request does not decode: {error}")
        })?;
        stats.decode = Some(start.elapsed());
        let same = encodes_to(&decoded, &bytes).map_err(|error| {
            format!("internal error: the decoded planned request does not encode: {error}")
        })?;
        if !same {
            return Err(format!(
                "internal error: the planned request does not round-trip ({} bytes encoded)",
                bytes.len()
            ));
        }
        decoded
    } else if record {
        let start = Instant::now();
        let decoded = decode(&bytes).map_err(|error| {
            format!("internal error: the planned request does not decode: {error}")
        })?;
        stats.decode = Some(start.elapsed());
        drop(decoded);
        request
    } else {
        request
    };
    drop(bytes);
    perf_line(&format!(
        "nodes={} bytes={} meta={} chunks={} encode_ms={:.3} decode_ms={} roundtrip={}",
        stats.nodes,
        stats.bytes,
        stats.meta_bytes,
        stats.chunks,
        stats.encode.as_secs_f64() * 1000.0,
        stats.decode.map_or_else(
            || "-".to_owned(),
            |d| format!("{:.3}", d.as_secs_f64() * 1000.0)
        ),
        u8::from(roundtrip),
    ));
    crate::snapshot::record::note_plan(Ok(stats));
    Ok(result)
}

/// `PERF-PLAN ...` on stderr under `BAZEL_RS_TIMING` (like PERF-REPO).
fn perf_line(text: &str) {
    if crate::diagnostic_switch(std::env::var_os("BAZEL_RS_TIMING")) {
        crate::snapshot::diag::line(&format!("PERF-PLAN {text}"));
    }
}

// ---- serde field helpers ---------------------------------------------------

/// A path as its raw bytes.
struct PathRef<'a>(&'a Path);

impl Serialize for PathRef<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(self.0.as_os_str().as_bytes())
    }
}

/// A byte string as bytes (serde's `Vec<u8>` is a sequence of numbers).
struct BytesRef<'a>(&'a [u8]);

impl Serialize for BytesRef<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(self.0)
    }
}

/// Owned bytes decoded from a byte string (a path's or a content's).
struct Bytes(Vec<u8>);

struct BytesVisitor;

impl Visitor<'_> for BytesVisitor {
    type Value = Vec<u8>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a byte string")
    }

    fn visit_bytes<E: serde::de::Error>(self, value: &[u8]) -> Result<Vec<u8>, E> {
        Ok(value.to_vec())
    }

    fn visit_byte_buf<E: serde::de::Error>(self, value: Vec<u8>) -> Result<Vec<u8>, E> {
        Ok(value)
    }
}

impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_byte_buf(BytesVisitor).map(Bytes)
    }
}

impl Bytes {
    fn path(self) -> PathBuf {
        PathBuf::from(OsString::from_vec(self.0))
    }
}

/// `PathBuf` as raw bytes.
pub(crate) mod path {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(value: &Path, serializer: S) -> Result<S::Ok, S::Error> {
        PathRef(value).serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<PathBuf, D::Error> {
        Bytes::deserialize(deserializer).map(Bytes::path)
    }
}

/// `Option<PathBuf>` as raw bytes.
pub(crate) mod optional_path {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(
        value: &Option<PathBuf>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value.as_deref().map(PathRef).serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<PathBuf>, D::Error> {
        Option::<Bytes>::deserialize(deserializer).map(|value| value.map(Bytes::path))
    }
}

/// `Vec<PathBuf>` as raw bytes.
pub(crate) mod paths {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(
        value: &[PathBuf],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(value.iter().map(|path| PathRef(path)))
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<PathBuf>, D::Error> {
        Vec::<Bytes>::deserialize(deserializer)
            .map(|value| value.into_iter().map(Bytes::path).collect())
    }
}

/// `BTreeSet<PathBuf>` as raw bytes (in set order).
pub(crate) mod path_set {
    use super::*;
    use std::collections::BTreeSet;

    pub(crate) fn serialize<S: Serializer>(
        value: &BTreeSet<PathBuf>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(value.iter().map(|path| PathRef(path)))
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeSet<PathBuf>, D::Error> {
        Vec::<Bytes>::deserialize(deserializer)
            .map(|value| value.into_iter().map(Bytes::path).collect())
    }
}

/// `Vec<(PathBuf, PathBuf)>` (an action's source inputs) as raw bytes.
pub(crate) mod path_pairs {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(
        value: &[(PathBuf, PathBuf)],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(value.iter().map(|(a, b)| (PathRef(a), PathRef(b))))
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<(PathBuf, PathBuf)>, D::Error> {
        Vec::<(Bytes, Bytes)>::deserialize(deserializer).map(|value| {
            value
                .into_iter()
                .map(|(a, b)| (a.path(), b.path()))
                .collect()
        })
    }
}

/// `Vec<(PathBuf, Vec<u8>)>` (parameter files) as raw bytes.
pub(crate) mod path_contents {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(
        value: &[(PathBuf, Vec<u8>)],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(value.iter().map(|(a, b)| (PathRef(a), BytesRef(b))))
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<(PathBuf, Vec<u8>)>, D::Error> {
        Vec::<(Bytes, Bytes)>::deserialize(deserializer)
            .map(|value| value.into_iter().map(|(a, b)| (a.path(), b.0)).collect())
    }
}

/// `Vec<(String, Option<PathBuf>)>` (runfiles entries) with raw-byte paths.
pub(crate) mod entries {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(
        value: &[(String, Option<PathBuf>)],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(
            value
                .iter()
                .map(|(name, path)| (name, path.as_deref().map(PathRef))),
        )
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<(String, Option<PathBuf>)>, D::Error> {
        Vec::<(String, Option<Bytes>)>::deserialize(deserializer).map(|value| {
            value
                .into_iter()
                .map(|(name, path)| (name, path.map(Bytes::path)))
                .collect()
        })
    }
}

/// `Vec<u8>` as one byte string.
pub(crate) mod bytes {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        BytesRef(value).serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        Bytes::deserialize(deserializer).map(|value| value.0)
    }
}

/// `Vec<(Vec<u8>, Vec<u8>)>` (template substitutions) as byte strings.
pub(crate) mod byte_pairs {
    use super::*;

    type Pairs = Vec<(Vec<u8>, Vec<u8>)>;

    pub(crate) fn serialize<S: Serializer>(
        value: &[(Vec<u8>, Vec<u8>)],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(value.iter().map(|(a, b)| (BytesRef(a), BytesRef(b))))
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Pairs, D::Error> {
        Vec::<(Bytes, Bytes)>::deserialize(deserializer)
            .map(|value| value.into_iter().map(|(a, b)| (a.0, b.0)).collect())
    }
}

/// The compiled-in blobs an action may embed (crate::action::Action::
/// embedded_inputs), by tag: the tag is the index.
fn embedded_blobs() -> [&'static [u8]; 1] {
    [crate::builtin_tools::GENRULE_SETUP]
}

/// `Vec<(PathBuf, &'static [u8])>` (embedded inputs): each blob as the tag
/// of a compiled-in blob, mapped back to the static bytes when decoded.
pub(crate) mod embedded {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(
        value: &[(PathBuf, &'static [u8])],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let blobs = embedded_blobs();
        let mut tagged = Vec::with_capacity(value.len());
        for (destination, bytes) in value {
            let tag = blobs
                .iter()
                .position(|blob| *blob == *bytes)
                .ok_or_else(|| {
                    S::Error::custom(format!(
                        "embedded input {} is not a compiled-in blob",
                        destination.display()
                    ))
                })?;
            tagged.push((PathRef(destination), tag as u8));
        }
        serializer.collect_seq(tagged)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<(PathBuf, &'static [u8])>, D::Error> {
        let blobs = embedded_blobs();
        Vec::<(Bytes, u8)>::deserialize(deserializer)?
            .into_iter()
            .map(|(destination, tag)| {
                let blob = blobs.get(usize::from(tag)).copied().ok_or_else(|| {
                    serde::de::Error::custom(format!("unknown embedded input tag {tag}"))
                })?;
                Ok((destination.path(), blob))
            })
            .collect()
    }
}

/// A Starlark `File` value's data (crate::starlark_types::File derives
/// starlark's always-failing `Serialize`, so it is encoded field by field).
pub(crate) mod file {
    use super::*;
    use crate::starlark_types::File;

    pub(crate) fn serialize<S: Serializer>(value: &File, serializer: S) -> Result<S::Ok, S::Error> {
        // Every field by name (no `..`): a new field of File is a compile
        // error here until the encoding carries it (deserialize builds the
        // value with a struct literal, which is exhaustive too).
        let File {
            path,
            short_path,
            owner,
            is_source,
            is_directory,
        } = value;
        (path, short_path, owner, is_source, is_directory).serialize(serializer)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<File, D::Error> {
        let (path, short_path, owner, is_source, is_directory) =
            <(String, String, String, bool, bool)>::deserialize(deserializer)?;
        Ok(File {
            path,
            short_path,
            owner,
            is_source,
            is_directory,
        })
    }
}

/// An Args `map_each` function: an in-memory Starlark value with no
/// encoding. `None` encodes as nothing; `Some` makes the request
/// unencodable (the snapshot blocker `map-each-deferred`, PLAN-L2 §1b).
pub(crate) mod map_each {
    use super::*;
    use starlark::values::OwnedFrozenValue;

    pub(crate) fn serialize<S: Serializer>(
        value: &Option<OwnedFrozenValue>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            None => serializer.serialize_unit(),
            Some(_) => Err(S::Error::custom(
                "an Args map_each function expands a tree artifact at execution time",
            )),
        }
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<OwnedFrozenValue>, D::Error> {
        <()>::deserialize(deserializer).map(|()| None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{Action, ActionExtra, Transform, WriteAction};
    use crate::command_line::{
        CommandLineSpec, DeferredVector, Group, Item, ParamFileInfo, ParamFileType, Part,
        VectorShape,
    };
    use crate::label::Label;
    use crate::request::{
        ConfigurationSummary, PlannedPath, PlannedRun, PlannedTest, Produced, RequestCommand,
    };
    use crate::scheduler::{Deferred, DeferredSpec, Dependency, TreeListings};
    use crate::starlark_types::File;
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Arc;

    fn raw(bytes: &[u8]) -> PathBuf {
        PathBuf::from(OsString::from_vec(bytes.to_vec()))
    }

    fn tree() -> File {
        File {
            path: "bazel-out/k8-fastbuild/bin/pkg/dir".into(),
            short_path: "pkg/dir".into(),
            owner: "//pkg:t".into(),
            is_source: false,
            is_directory: true,
        }
    }

    fn vector(map_each: Option<starlark::values::OwnedFrozenValue>) -> Group {
        Group::Deferred(Arc::new(DeferredVector {
            shape: VectorShape {
                arg_name: Some("--files".into()),
                format_each: Some("f=%s".into()),
                join_with: None,
                uniquify: true,
                ..VectorShape::default()
            },
            items: vec![Item::Strings(vec!["lit".into()]), Item::Directory(tree())],
            map_each,
        }))
    }

    fn command_line(map_each: Option<starlark::values::OwnedFrozenValue>) -> Deferred {
        Deferred::new(DeferredSpec::CommandLine {
            spec: CommandLineSpec {
                executable: vec!["/bin/bash".into(), "-c".into(), "true".into()],
                parts: vec![Part {
                    groups: vec![
                        Group::Strings(vec!["--a".into(), "b c".into()]),
                        vector(map_each),
                    ],
                    flag_per_line: false,
                    param: Some(ParamFileInfo {
                        flag_format: "@%s".into(),
                        always: true,
                        file_type: ParamFileType::Shell,
                        flags_only: false,
                    }),
                }],
                primary_output: "bazel-out/k8-fastbuild/bin/pkg/out".into(),
                max_length: crate::command_line::MIN_PARAM_FILE_SIZE,
            },
            progress: "Action pkg/out".into(),
            extra_param_files: vec![(PathBuf::from("pkg/out.runfiles/empty"), Vec::new())],
        })
    }

    fn label(text: &str) -> Label {
        Label::parse_canonical(text).unwrap()
    }

    fn produced(real: &str, producer: Option<(&str, usize)>) -> Produced {
        Produced {
            real: PathBuf::from(real),
            producer: producer.map(|(id, index)| (id.to_owned(), index)),
        }
    }

    /// A request with every planned shape: non-UTF-8 paths, an embedded
    /// input, every transform, deferred command lines and writes, the
    /// status placeholders, run plans, tests, diagnostics.
    fn sample() -> PlannedRequest {
        let process = Action {
            id: "//pkg:gen".into(),
            command: "source x; echo hi > $@".into(),
            inputs: vec![
                (raw(b"/ws/pkg/\xffin"), raw(b"pkg/\xffin")),
                ("/ws/pkg/b".into(), "pkg/b".into()),
            ],
            outputs: vec![raw(b"bazel-out/k8-fastbuild/bin/pkg/\xfeout")],
            arguments: vec!["x".into()],
            environment: BTreeMap::from([("A".into(), "b".into())]),
            shell_strict: true,
            extra: ActionExtra {
                tree_outputs: BTreeSet::from([PathBuf::from("bazel-out/t")]),
                tree_inputs: BTreeSet::from([PathBuf::from("pkg/dir")]),
                param_files: vec![("pkg/p.params".into(), vec![0, 255, b'\n'])],
                inherited_environment: BTreeSet::from(["PATH".to_owned()]),
                client_environment: BTreeMap::from([("PATH".to_owned(), OsString::from("/x"))]),
                execution_info: BTreeMap::from([("no-remote".into(), String::new())]),
                mnemonic: "Genrule".into(),
                transform: None,
                constant_metadata: vec!["bazel-out/volatile-status.txt".into()],
            },
            shell_pad: true,
            executable_file: false,
            execution_info: BTreeSet::from(["local".to_owned()]),
            embedded_inputs: vec![(
                crate::builtin_tools::GENRULE_SETUP_EXEC_PATH.into(),
                crate::builtin_tools::GENRULE_SETUP,
            )],
            remote_unsupported: Some("exec_properties".into()),
            program: None,
            write: None,
        };
        let transform = |id: &str, transform: Transform| PlannedAction {
            action: Action {
                id: id.into(),
                outputs: vec![PathBuf::from(format!("bazel-out/{id}"))],
                extra: ActionExtra {
                    transform: Some(transform),
                    ..ActionExtra::default()
                },
                ..Action::default()
            },
            dependencies: vec![Dependency {
                action_id: "//pkg:gen".into(),
                output_index: 0,
                destination: raw(b"bazel-out/k8-fastbuild/bin/pkg/\xfeout"),
            }],
            deferred: None,
        };
        let nodes = vec![
            PlannedAction {
                action: process,
                dependencies: Vec::new(),
                deferred: Some(command_line(None)),
            },
            transform(
                "symlink",
                Transform::Symlink {
                    target: "pkg/a".into(),
                    require_executable: true,
                    progress: "Symlinking".into(),
                },
            ),
            transform(
                "template",
                Transform::Template {
                    template: "pkg/t".into(),
                    substitutions: vec![(b"{A}".to_vec(), vec![0xff])],
                    executable: true,
                },
            ),
            transform(
                "keyed",
                Transform::TemplateKeyed {
                    template: "pkg/t".into(),
                    substitutions: vec![(b"{T}".to_vec(), b"1".to_vec())],
                    keyed_substitutions: Vec::new(),
                    executable: false,
                    volatile: true,
                    callback_digest: "f".into(),
                },
            ),
            transform(
                "unsupported",
                Transform::Unsupported {
                    message: "m".into(),
                },
            ),
            transform("build-info", Transform::BuildInfo { volatile: false }),
            transform(
                crate::request::STABLE_STATUS_ID,
                Transform::WorkspaceStatus { volatile: false },
            ),
            transform(
                crate::request::VOLATILE_STATUS_ID,
                Transform::WorkspaceStatus { volatile: true },
            ),
            transform(
                "manifest",
                Transform::RunfilesManifest {
                    entries: vec![
                        ("_main/a".into(), Some(raw(b"pkg/\xffa"))),
                        ("_main/e".into(), None),
                    ],
                },
            ),
            transform(
                "tree",
                Transform::RunfilesTree {
                    entries: vec![("_main/a".into(), Some("pkg/a".into()))],
                    manifest: "bazel-out/x.runfiles_manifest".into(),
                    executable: Some("bazel-out/x".into()),
                    create: true,
                },
            ),
            PlannedAction {
                action: Action {
                    id: "write".into(),
                    outputs: vec!["bazel-out/w".into()],
                    write: Some(WriteAction {
                        content: vec![0, 1, 2, 255],
                        executable: true,
                    }),
                    ..Action::default()
                },
                dependencies: Vec::new(),
                deferred: Some(Deferred::new(DeferredSpec::WriteArgs {
                    snapshot: crate::args::ArgsSnapshot {
                        groups: vec![vector(None)],
                        flag_per_line: true,
                        file_type: ParamFileType::Unquoted,
                        param: None,
                    },
                    extra_param_files: Vec::new(),
                })),
            },
        ];
        PlannedRequest {
            command: RequestCommand::Test {
                build: vec![label("//pkg:t"), label("@@r+//x:y")],
                tests: vec![PlannedTest {
                    label: label("//pkg:t"),
                    info: crate::test_command::TestRuleInfo {
                        rule_class: "sh_test".into(),
                        size: crate::test_command::TestSize::Small,
                        timeout: crate::test_command::TestTimeout::Short,
                        flaky: true,
                        shard_count: -1,
                        local: false,
                        tags: vec!["unit".into()],
                    },
                    location: Some("/ws/pkg/BUILD:3:8".into()),
                }],
            },
            execute: true,
            nodes,
            requested: vec![(
                label("//pkg:t"),
                produced("bazel-out/t", Some(("//pkg:gen", 0))),
            )],
            aspect_reports: vec![(
                "//pkg:a.bzl%a".into(),
                label("//pkg:t"),
                vec![produced("pkg/src", None)],
            )],
            non_empty_targets: 1,
            run_plans: vec![(
                label("//pkg:t"),
                PlannedRun {
                    executable: produced("bazel-out/t.sh", Some(("write", 0))),
                    executable_exec: "bazel-out/k8-fastbuild/bin/pkg/t.sh".into(),
                    executable_short_path: "pkg/t.sh".into(),
                    tree: Some((
                        produced("bazel-out/t.runfiles/MANIFEST", Some(("tree", 0))),
                        PlannedPath("bazel-out/k8-fastbuild/bin/pkg/t.sh.runfiles/MANIFEST".into()),
                    )),
                    entries: Some(vec![
                        ("_main/pkg/a".into(), Some(produced("pkg/a", None))),
                        ("_main/empty".into(), None),
                    ]),
                    settings: crate::runfiles::RunSettings {
                        args: vec!["--x".into()],
                        environment: vec![("E".into(), "v".into())],
                        inherited: vec!["HOME".into()],
                        test: true,
                        test_requirements: Some(crate::execution_info::TestRequirements {
                            requirements: vec![("requires-network".into(), String::new())],
                            exec_group: "test".into(),
                        }),
                    },
                    configuration: 0,
                },
            )],
            configurations: vec![ConfigurationSummary {
                mnemonic: "k8-fastbuild".into(),
                action_env: crate::action_env::ActionEnvironment {
                    fixed: BTreeMap::from([("PATH".into(), "/bin".into())]),
                    inherited: BTreeSet::from(["LD_LIBRARY_PATH".into()]),
                },
                test_env: crate::action_env::ActionEnvironment::default(),
            }],
            warnings: vec!["w \u{1}".into()],
            errors: vec!["e".into()],
            build_info: vec!["keyed".into()],
        }
    }

    /// What an expansion produced: arguments, parameter files, written bytes.
    type Expansion = (Vec<String>, Vec<(PathBuf, Vec<u8>)>, Vec<u8>);

    /// Expand a deferred completion against one listing.
    fn applied(deferred: &Deferred, write: bool) -> Expansion {
        let listings: TreeListings = BTreeMap::from([(
            PathBuf::from(tree().path),
            vec![PathBuf::from("a.txt"), PathBuf::from("sub/b.txt")],
        )]);
        let mut action = Action {
            write: write.then(|| WriteAction {
                content: Vec::new(),
                executable: false,
            }),
            ..Action::default()
        };
        deferred.apply(&listings, &mut action).unwrap();
        (
            action.arguments,
            action.extra.param_files,
            action.write.map(|write| write.content).unwrap_or_default(),
        )
    }

    #[test]
    fn every_planned_shape_round_trips_byte_for_byte() {
        let request = sample();
        let bytes = encode(&request).unwrap();
        assert_eq!(encode(&request).unwrap(), bytes, "deterministic");
        let decoded = decode(&bytes).unwrap();
        assert_eq!(encode(&decoded).unwrap(), bytes, "re-encoded");
        let process = &decoded.nodes[0].action;
        assert_eq!(
            process.inputs[0],
            (raw(b"/ws/pkg/\xffin"), raw(b"pkg/\xffin"))
        );
        assert_eq!(
            process.outputs,
            [raw(b"bazel-out/k8-fastbuild/bin/pkg/\xfeout")]
        );
        assert_eq!(process.extra.param_files[0].1, [0, 255, b'\n']);
        // The embedded input is the compiled-in blob.
        assert_eq!(
            process.embedded_inputs[0].1,
            crate::builtin_tools::GENRULE_SETUP
        );
        // Client environment values are never encoded (finalize resolves them).
        assert!(process.extra.client_environment.is_empty());
        assert_eq!(
            process.extra.inherited_environment,
            request.nodes[0].action.extra.inherited_environment
        );
        assert_eq!(decoded.command, request.command);
        assert_eq!(decoded.requested, request.requested);
        assert_eq!(decoded.aspect_reports, request.aspect_reports);
        assert_eq!(decoded.run_plans, request.run_plans);
        assert_eq!(decoded.configurations, request.configurations);
        assert_eq!(decoded.warnings, request.warnings);
        assert_eq!(decoded.errors, request.errors);
        assert_eq!(decoded.build_info, request.build_info);
        assert_eq!(decoded.non_empty_targets, 1);
        assert!(decoded.execute);
        // The decoded completions expand exactly like the planned ones.
        for (index, write) in [(0, false), (10, true)] {
            assert_eq!(
                applied(decoded.nodes[index].deferred.as_ref().unwrap(), write),
                applied(request.nodes[index].deferred.as_ref().unwrap(), write)
            );
        }
        let (arguments, param_files, _) =
            applied(decoded.nodes[0].deferred.as_ref().unwrap(), false);
        assert_eq!(arguments, ["@bazel-out/k8-fastbuild/bin/pkg/out-0.params"]);
        assert_eq!(
            String::from_utf8_lossy(&param_files[0].1),
            "--a\n'b c'\n--files\n'f=lit'\n'f=bazel-out/k8-fastbuild/bin/pkg/dir/a.txt'\n'f=bazel-out/k8-fastbuild/bin/pkg/dir/sub/b.txt'\n"
        );
        assert_eq!(
            param_files[1],
            (PathBuf::from("pkg/out.runfiles/empty"), Vec::new())
        );
    }

    #[test]
    fn large_plans_encode_in_fixed_chunks_and_decode_them_in_parallel() {
        let mut request = sample();
        let node = request.nodes[10].clone();
        request.nodes = (0..2 * CHUNK + 5)
            .map(|index| {
                let mut node = node.clone();
                node.action.id = format!("//pkg:w#{index}");
                node.action.write = Some(WriteAction {
                    content: vec![b'x'; 2048],
                    executable: false,
                });
                node
            })
            .collect();
        let bytes = encode(&request).unwrap();
        let sections = sections(&bytes).unwrap();
        assert_eq!(sections.nodes.len(), 3);
        assert!(
            sections
                .nodes
                .iter()
                .map(|chunk| chunk.len())
                .sum::<usize>()
                >= PARALLEL_DECODE_BYTES
        );
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.nodes.len(), 2 * CHUNK + 5);
        assert_eq!(
            decoded.nodes[2 * CHUNK + 4].action.id,
            format!("//pkg:w#{}", 2 * CHUNK + 4)
        );
        assert_eq!(encode(&decoded).unwrap(), bytes);
    }

    /// The chunk work runs on at most [`MAX_THREADS`] threads, results in
    /// item order; a panicking item is a `None`, never a crash.
    #[test]
    fn chunk_work_runs_on_a_bounded_number_of_threads() {
        let items: Vec<usize> = (0..64).collect();
        let threads = Mutex::new(std::collections::HashSet::new());
        let results = on_threads(&items, |item| {
            threads.lock().unwrap().insert(std::thread::current().id());
            std::thread::sleep(Duration::from_millis(2));
            assert_ne!(*item, 13, "item 13 panics");
            item * 2
        });
        let used = threads.lock().unwrap().len();
        assert!((1..=MAX_THREADS).contains(&used), "{used} threads");
        for (index, result) in results.iter().enumerate() {
            if index == 13 {
                assert!(result.is_none());
            } else {
                assert_eq!(*result, Some(index * 2));
            }
        }
        assert!(on_threads(&[] as &[usize], |item| *item).is_empty());
    }

    /// The round trip's section-by-section check sees any difference.
    #[test]
    fn the_round_trip_check_compares_every_section() {
        let mut request = sample();
        let bytes = encode(&request).unwrap();
        assert!(encodes_to(&request, &bytes).unwrap());
        request.warnings.push("another".into());
        assert!(!encodes_to(&request, &bytes).unwrap());
        let mut request = sample();
        request.nodes[3].action.id.push('x');
        assert!(!encodes_to(&request, &bytes).unwrap());
        let mut request = sample();
        request.nodes.pop();
        assert!(!encodes_to(&request, &bytes).unwrap());
    }

    #[test]
    fn damaged_encodings_are_refused() {
        let bytes = encode(&sample()).unwrap();
        assert!(decode(b"BZRSPLN0").unwrap_err().contains("bad magic"));
        assert!(
            decode(&bytes[..bytes.len() - 1])
                .unwrap_err()
                .contains("truncated")
        );
        let mut other = bytes.clone();
        other[8..12].copy_from_slice(&(FORMAT + 1).to_le_bytes());
        assert!(decode(&other).unwrap_err().contains("format"));
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(decode(&trailing).unwrap_err().contains("trailing"));
        // A damaged action chunk fails to decode, never panics.
        let mut damaged = bytes.clone();
        let last = damaged.len() - 1;
        damaged[last] ^= 0xff;
        let _ = decode(&damaged);
    }

    /// A Starlark `map_each` stays an in-memory value: the request cannot
    /// be encoded, and the action is a snapshot blocker.
    #[test]
    fn a_map_each_makes_the_request_unencodable() {
        let frozen = starlark::environment::Module::with_temp_heap(|module| {
            {
                let mut eval = starlark::eval::Evaluator::new(&module);
                let ast = starlark::syntax::AstModule::parse(
                    "m.bzl",
                    "def f(x):\n    return x.path\n".to_owned(),
                    &starlark::syntax::Dialect::Standard,
                )
                .unwrap();
                eval.eval_module(ast, &starlark::environment::Globals::standard())
                    .unwrap();
            }
            module.freeze()
        })
        .unwrap();
        let mut request = sample();
        request.nodes[0].deferred = Some(command_line(Some(frozen.get("f").unwrap())));
        assert!(request.nodes[0].deferred.as_ref().unwrap().uses_map_each());
        let error = encode(&request).unwrap_err().to_string();
        assert!(
            error.contains("//pkg:gen") && error.contains("map_each"),
            "{error}"
        );
        let blockers = request.blockers();
        assert!(blockers.iter().any(
            |blocker| blocker.taint == crate::observed_fs::Taint::MapEach
                && blocker.detail == "//pkg:gen"
        ));
        assert!(blockers.iter().any(|blocker| blocker.taint
            == crate::observed_fs::Taint::BuildInfo
            && blocker.detail == "keyed"));
    }

    /// The bound attributes of a replay record (crate::snapshot::replay)
    /// round-trip through postcard (L2-W5 saves them).
    #[test]
    fn attribute_values_round_trip() {
        use crate::custom_rule::AttrValue;
        let values = vec![
            AttrValue::String("s".into()),
            AttrValue::Strings(vec!["a".into(), "b".into()]),
            AttrValue::StringListDict(vec![("k".into(), vec!["v".into()])]),
            AttrValue::StringDict(vec![("k".into(), "v".into())]),
            AttrValue::Int(-3),
            AttrValue::Bool(true),
            AttrValue::Label(Some("@@r+//p:t".into())),
            AttrValue::Label(None),
            AttrValue::Labels(vec!["//p:t".into()]),
            AttrValue::Output(Some("o".into())),
            AttrValue::Outputs(Vec::new()),
            AttrValue::Ints(vec![1, 2]),
            AttrValue::LabelKeyedStringDict(vec![("//p:t".into(), "v".into())]),
            AttrValue::StringKeyedLabelDict(vec![("k".into(), "//p:t".into())]),
            AttrValue::LabelListDict(vec![("k".into(), vec!["//p:t".into()])]),
        ];
        let bytes = postcard::to_stdvec(&values).unwrap();
        assert_eq!(
            postcard::from_bytes::<Vec<AttrValue>>(&bytes).unwrap(),
            values
        );
    }

    #[test]
    fn only_compiled_in_blobs_embed() {
        let mut request = sample();
        static OTHER: &[u8] = b"not a compiled-in blob";
        request.nodes[0].action.embedded_inputs = vec![("x".into(), OTHER)];
        assert!(
            encode(&request)
                .unwrap_err()
                .to_string()
                .contains("compiled-in")
        );
    }
}
