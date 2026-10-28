//! The snapshot file format (PLAN-L2 §1d; binary only, the user's
//! directive): a fixed-size header, then sections, each with its own
//! checksum, so a reader validates the inputs before it touches the plan.
//!
//! Header (352 bytes, little-endian):
//!
//! | offset | content |
//! |---|---|
//! | 0 | magic `BZRSSNP1` |
//! | 8 | format (u32, crate::snapshot::key::FORMAT), flags (u32, 0) |
//! | 16 | the bazel-rs binary identity (32 bytes, crate::snapshot::exe_id) |
//! | 48 | the full key G (32 bytes; the file name holds its first 16) |
//! | 80 | created, ns since the epoch (i64) |
//! | 88 | section count (u32), reserved (u32) |
//! | 96 | six section slots of 40 bytes: kind (u32), reserved (u32), offset (u64), length (u64), the first 16 bytes of blake3 of the section |
//! | 336 | the first 16 bytes of blake3 of bytes 0..336 |
//!
//! Sections, in file order:
//!
//! * INPUTS — what a hit validates and replays (postcard): the window
//!   start, a path table (sorted, front-coded: shared-prefix length u16,
//!   suffix bytes), the filesystem observations by path index, the
//!   environment digests, the verified repository trees with the integrity
//!   entries the policy validates (per tree: root and manifest; full:
//!   every entry), the local repository rules' replay records and the
//!   pinned tables their replay answers from.
//! * EVENTS — what planning printed: the window's stderr bytes.
//! * PLAN — the planned request (crate::snapshot::codec, self-framed),
//!   stored compressed (P3, [`compress_plan`]: independent deflate frames of
//!   4 MiB, compressed and inflated on threads; [`Reader::section`] of
//!   crate::snapshot::store inflates it).
//! * STATS — counts, sizes and times, for diagnostics.
//!
//! The payloads hold no hash order and no per-process ids: the same
//! recording and plan encode to the same bytes. Paths and environment
//! names are raw bytes (non-UTF-8 survives). Snapshots never survive a
//! binary change (the identity is in the key and the header), so the wire
//! types simply mirror today's in-memory ones: no migrations.

use crate::custom_rule::AttrValue;
use crate::label::Label;
use crate::observed_fs::{
    EnvObservation, FileKind, IntegrityEntry, Observation, Op, Outcome, Projection, Recording,
    RepositoryRecord, StatKey,
};
use crate::snapshot::key::Key;
use crate::snapshot::replay::{
    DefinitionSource, PinnedMapping, PinnedRoot, PinnedTables, ReplayRecord,
};
use serde::de::{Deserializer, Visitor};
use serde::{Deserialize, Serialize, Serializer};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::time::Duration;

pub const MAGIC: &[u8; 8] = b"BZRSSNP1";
pub const HEADER_LEN: usize = 352;
const SLOTS: usize = 6;
const SLOT_LEN: usize = 40;
const SLOTS_AT: usize = 96;
const HEADER_SUM_AT: usize = SLOTS_AT + SLOTS * SLOT_LEN;

/// The kinds of section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Inputs = 1,
    Events = 2,
    Plan = 3,
    Stats = 4,
    /// The dependency index of incremental analysis
    /// (crate::snapshot::incremental, compressed like the plan); a hit
    /// never reads it.
    Index = 5,
}

impl Section {
    pub const ALL: [Section; 5] = [
        Section::Inputs,
        Section::Events,
        Section::Plan,
        Section::Stats,
        Section::Index,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Inputs => "INPUTS",
            Self::Events => "EVENTS",
            Self::Plan => "PLAN",
            Self::Stats => "STATS",
            Self::Index => "INDEX",
        }
    }
}

/// One section slot of the header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    pub kind: u32,
    pub offset: u64,
    pub len: u64,
    pub checksum: [u8; 16],
}

/// A decoded header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub format: u32,
    pub binary: [u8; 32],
    pub key: [u8; 32],
    pub created_ns: i64,
    pub slots: Vec<Slot>,
}

/// The first 16 bytes of blake3 of `bytes`.
pub fn checksum(bytes: &[u8]) -> [u8; 16] {
    let hash = blake3::hash(bytes);
    hash.as_bytes()[..16].try_into().expect("16 bytes")
}

impl Header {
    /// The header of a file holding `sections` (kind, bytes) in this order
    /// right after the header.
    pub fn new(key: &Key, created_ns: i64, sections: &[(Section, &[u8])]) -> Self {
        let mut offset = HEADER_LEN as u64;
        let slots = sections
            .iter()
            .map(|(kind, bytes)| {
                let slot = Slot {
                    kind: *kind as u32,
                    offset,
                    len: bytes.len() as u64,
                    checksum: checksum(bytes),
                };
                offset += bytes.len() as u64;
                slot
            })
            .collect();
        Self {
            format: crate::snapshot::key::FORMAT,
            binary: key.binary,
            key: key.g,
            created_ns,
            slots,
        }
    }

    pub fn encode(&self) -> [u8; HEADER_LEN] {
        assert!(self.slots.len() <= SLOTS, "at most {SLOTS} sections");
        let mut bytes = [0u8; HEADER_LEN];
        bytes[..8].copy_from_slice(MAGIC);
        bytes[8..12].copy_from_slice(&self.format.to_le_bytes());
        bytes[16..48].copy_from_slice(&self.binary);
        bytes[48..80].copy_from_slice(&self.key);
        bytes[80..88].copy_from_slice(&self.created_ns.to_le_bytes());
        bytes[88..92].copy_from_slice(&(self.slots.len() as u32).to_le_bytes());
        for (index, slot) in self.slots.iter().enumerate() {
            let at = SLOTS_AT + index * SLOT_LEN;
            bytes[at..at + 4].copy_from_slice(&slot.kind.to_le_bytes());
            bytes[at + 8..at + 16].copy_from_slice(&slot.offset.to_le_bytes());
            bytes[at + 16..at + 24].copy_from_slice(&slot.len.to_le_bytes());
            bytes[at + 24..at + 40].copy_from_slice(&slot.checksum);
        }
        let sum = checksum(&bytes[..HEADER_SUM_AT]);
        bytes[HEADER_SUM_AT..].copy_from_slice(&sum);
        bytes
    }

    /// Decode and check a header (magic, header checksum, format, slots
    /// inside a file of `file_len` bytes).
    pub fn decode(bytes: &[u8], file_len: u64) -> Result<Self, String> {
        if bytes.len() < HEADER_LEN {
            return Err("truncated header".to_owned());
        }
        let bytes = &bytes[..HEADER_LEN];
        if &bytes[..8] != MAGIC {
            return Err("not a snapshot (bad magic)".to_owned());
        }
        if bytes[HEADER_SUM_AT..] != checksum(&bytes[..HEADER_SUM_AT]) {
            return Err("damaged header (checksum)".to_owned());
        }
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4"));
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8"));
        let format = u32_at(8);
        if format != crate::snapshot::key::FORMAT {
            return Err(format!(
                "snapshot format {format}, this binary reads {}",
                crate::snapshot::key::FORMAT
            ));
        }
        let count = u32_at(88) as usize;
        if count > SLOTS {
            return Err(format!("{count} sections (at most {SLOTS})"));
        }
        let mut slots = Vec::with_capacity(count);
        for index in 0..count {
            let at = SLOTS_AT + index * SLOT_LEN;
            let slot = Slot {
                kind: u32_at(at),
                offset: u64_at(at + 8),
                len: u64_at(at + 16),
                checksum: bytes[at + 24..at + 40].try_into().expect("16"),
            };
            if slot.offset < HEADER_LEN as u64
                || slot
                    .offset
                    .checked_add(slot.len)
                    .is_none_or(|end| end > file_len)
            {
                return Err(format!(
                    "section {} lies outside the file (truncated?)",
                    slot.kind
                ));
            }
            slots.push(slot);
        }
        Ok(Self {
            format,
            binary: bytes[16..48].try_into().expect("32"),
            key: bytes[48..80].try_into().expect("32"),
            created_ns: u64_at(80) as i64,
            slots,
        })
    }

    pub fn slot(&self, kind: Section) -> Option<&Slot> {
        self.slots.iter().find(|slot| slot.kind == kind as u32)
    }
}

// ---- wire helpers ----------------------------------------------------------

/// A borrowed byte string (serde's `&[u8]` is a sequence of numbers).
#[derive(Clone, Copy, Debug)]
struct Bytes<'a>(&'a [u8]);

impl Serialize for Bytes<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(self.0)
    }
}

struct BorrowedBytes;

impl<'de> Visitor<'de> for BorrowedBytes {
    type Value = &'de [u8];

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a borrowed byte string")
    }

    fn visit_borrowed_bytes<E: serde::de::Error>(self, value: &'de [u8]) -> Result<Self::Value, E> {
        Ok(value)
    }
}

impl<'de: 'a, 'a> Deserialize<'de> for Bytes<'a> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_bytes(BorrowedBytes).map(Bytes)
    }
}

fn path_of(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct KeyWire {
    dev: u64,
    ino: u64,
    size: u64,
    mtime_ns: i64,
    ctime_ns: i64,
}

impl From<&StatKey> for KeyWire {
    fn from(key: &StatKey) -> Self {
        Self {
            dev: key.dev,
            ino: key.ino,
            size: key.size,
            mtime_ns: key.mtime_ns,
            ctime_ns: key.ctime_ns,
        }
    }
}

impl From<KeyWire> for StatKey {
    fn from(key: KeyWire) -> Self {
        Self {
            dev: key.dev,
            ino: key.ino,
            size: key.size,
            mtime_ns: key.mtime_ns,
            ctime_ns: key.ctime_ns,
        }
    }
}

fn kind_code(kind: FileKind) -> u8 {
    match kind {
        FileKind::File => 0,
        FileKind::Dir => 1,
        FileKind::Symlink => 2,
        FileKind::Other => 3,
    }
}

fn kind_of(code: u8) -> Result<FileKind, String> {
    Ok(match code {
        0 => FileKind::File,
        1 => FileKind::Dir,
        2 => FileKind::Symlink,
        3 => FileKind::Other,
        other => return Err(format!("unknown file kind {other}")),
    })
}

const TREE_KINDS: [&str; 4] = ["registry", "direct", "git", "builtin"];
const FACILITIES: [&str; 3] = ["download", "execute", "which"];

#[derive(Serialize, Deserialize)]
enum ProjectionWire<'a> {
    All,
    Directories,
    Glob { pattern: &'a str, relative: &'a str },
}

#[derive(Serialize, Deserialize)]
enum OpWire<'a> {
    Read,
    Stat,
    Lstat,
    ReadDir(#[serde(borrow)] ProjectionWire<'a>),
    Canon,
    ReadLink,
}

#[derive(Serialize, Deserialize)]
enum OutcomeWire<'a> {
    Error(i32),
    Read {
        sha256: [u8; 32],
        key: KeyWire,
        racy: bool,
    },
    Kind(u8),
    Link(#[serde(borrow)] Bytes<'a>),
    Listing {
        digest: [u8; 32],
        entries: u32,
        key: KeyWire,
        racy: bool,
    },
    Path(#[serde(borrow)] Bytes<'a>),
}

#[derive(Serialize, Deserialize)]
struct ObservationWire<'a> {
    path: u32,
    #[serde(borrow)]
    op: OpWire<'a>,
    #[serde(borrow)]
    outcome: OutcomeWire<'a>,
}

#[derive(Serialize, Deserialize)]
struct EnvWire<'a> {
    #[serde(borrow)]
    name: Bytes<'a>,
    digest: Option<[u8; 32]>,
}

#[derive(Serialize, Deserialize)]
struct EntryWire {
    path: u32,
    state: Result<(u8, KeyWire), i32>,
    racy: bool,
}

#[derive(Serialize, Deserialize)]
struct TreeWire<'a> {
    kind: u8,
    name: &'a str,
    root: u32,
    manifest: Option<u32>,
    entries: Vec<EntryWire>,
}

#[derive(Serialize, Deserialize)]
enum DefinitionWire<'a> {
    Direct { label: Label, symbol: &'a str },
    Ordinary { rule_id: &'a str },
}

#[derive(Serialize, Deserialize)]
struct ReplayWire<'a> {
    canonical: &'a str,
    original_name: &'a str,
    #[serde(borrow)]
    definition: DefinitionWire<'a>,
    identity: &'a str,
    definition_label: Label,
    attributes: BTreeMap<String, AttrValue>,
    #[serde(borrow)]
    root: Bytes<'a>,
    inputs_digest: [u8; 32],
    snapshot_digest: [u8; 32],
    execution_identity: &'a str,
    #[serde(borrow)]
    output: Bytes<'a>,
    environ_read: bool,
    #[serde(borrow)]
    facilities: Vec<&'a str>,
    elapsed_ns: u64,
}

#[derive(Serialize, Deserialize)]
#[allow(clippy::type_complexity)]
struct TablesWire<'a> {
    #[serde(borrow)]
    main_root: Bytes<'a>,
    #[serde(borrow)]
    source_cache: Bytes<'a>,
    #[serde(borrow)]
    roots: Vec<(&'a str, Bytes<'a>, bool)>,
    #[serde(borrow)]
    mappings: Vec<(&'a str, Vec<(&'a str, &'a str)>, Vec<&'a str>)>,
}

#[derive(Serialize, Deserialize)]
struct InputsWire<'a> {
    window_start_ns: i64,
    #[serde(borrow)]
    paths: Vec<(u16, Bytes<'a>)>,
    #[serde(borrow)]
    observations: Vec<ObservationWire<'a>>,
    #[serde(borrow)]
    env: Vec<EnvWire<'a>>,
    #[serde(borrow)]
    trees: Vec<TreeWire<'a>>,
    #[serde(borrow)]
    replay: Vec<ReplayWire<'a>>,
    #[serde(borrow)]
    tables: Option<TablesWire<'a>>,
    workspace_status: bool,
}

#[derive(Serialize, Deserialize)]
struct EventsWire<'a> {
    #[serde(borrow)]
    window_stderr: Bytes<'a>,
}

/// The STATS section: what the save measured (diagnostics only).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    /// Observations in the recording / in the INPUTS section.
    pub observations: u64,
    pub validated: u64,
    pub env: u64,
    pub trees: u64,
    pub integrity_entries: u64,
    pub replay_records: u64,
    pub window_stderr: u64,
    pub window_us: u64,
    pub plan_nodes: u64,
    pub plan_bytes: u64,
    pub inputs_bytes: u64,
    pub encode_us: u64,
    pub full_validation: bool,
}

// ---- PLAN compression (P3) ------------------------------------------------

/// The magic of a compressed PLAN section ([`compress_plan`]); an encoded
/// plan itself starts with crate::snapshot::codec::MAGIC.
pub const COMPRESSED_MAGIC: &[u8; 8] = b"BZRSPZ01";
/// Plan bytes per compressed frame (fixed: the stored bytes must not depend
/// on the machine).
const FRAME: usize = 4 << 20;
/// Threads compressing or inflating the frames of one plan at most.
const FRAME_THREADS: usize = 8;

/// Run `work` over `items` on up to [`FRAME_THREADS`] scoped threads (the
/// calling thread alone for one item), results in item order.
fn on_threads<T: Sync, R: Send>(items: &[T], work: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .clamp(1, FRAME_THREADS)
        .min(items.len());
    if threads <= 1 {
        return items.iter().map(work).collect();
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Option<R>>> =
        items.iter().map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(item) = items.get(index) else {
                        break;
                    };
                    let result = work(item);
                    *results[index]
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
                }
            });
        }
    });
    results
        .into_iter()
        .map(|result| {
            result
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .expect("every frame was processed")
        })
        .collect()
}

/// The stored form of the PLAN section (P3): [`COMPRESSED_MAGIC`], the
/// frame count (u32), per frame its plan bytes and stored bytes (u32 each),
/// then the frames — each [`FRAME`] plan bytes (the last fewer) deflated
/// at the fastest level, independently, so they compress and inflate on
/// threads. The same plan always gives the same bytes. On ~/code a plan
/// shrinks about 13 times (`//src/ts/...`: 230 MB to 18 MB): its actions
/// repeat the same input paths, arguments and ids over and over.
pub fn compress_plan(plan: &[u8]) -> Vec<u8> {
    let frames: Vec<&[u8]> = plan.chunks(FRAME).collect();
    let compressed = on_threads(&frames, |frame| deflate(frame));
    let mut stored =
        Vec::with_capacity(16 + 8 * frames.len() + compressed.iter().map(Vec::len).sum::<usize>());
    stored.extend_from_slice(COMPRESSED_MAGIC);
    stored.extend_from_slice(&(frames.len() as u32).to_le_bytes());
    for (frame, deflated) in frames.iter().zip(&compressed) {
        stored.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        stored.extend_from_slice(&(deflated.len() as u32).to_le_bytes());
    }
    for deflated in &compressed {
        stored.extend_from_slice(deflated);
    }
    stored
}

/// One part of a plan (crate::snapshot::codec::encode_parts_with) as
/// compressed frames of at most [`FRAME`] bytes: (size, deflated) — what
/// [`compress_plan`] does to a whole plan, here on the thread that encoded
/// the part, so the uncompressed part is freed right away (MEM4).
pub fn compress_part(part: Vec<u8>) -> Vec<(usize, Vec<u8>)> {
    part.chunks(FRAME)
        .map(|frame| (frame.len(), deflate(frame)))
        .collect()
}

/// The stored PLAN section of compressed parts ([`compress_part`]), in
/// order: [`compress_plan`]'s format, the frames following the parts (a
/// frame never spans two), so it inflates to the concatenated plan.
pub fn assemble_compressed(parts: Vec<Vec<(usize, Vec<u8>)>>) -> Vec<u8> {
    let frames: Vec<(usize, Vec<u8>)> = parts.into_iter().flatten().collect();
    let mut stored = Vec::with_capacity(
        16 + 8 * frames.len()
            + frames
                .iter()
                .map(|(_, deflated)| deflated.len())
                .sum::<usize>(),
    );
    stored.extend_from_slice(COMPRESSED_MAGIC);
    stored.extend_from_slice(&(frames.len() as u32).to_le_bytes());
    for (size, deflated) in &frames {
        stored.extend_from_slice(&(*size as u32).to_le_bytes());
        stored.extend_from_slice(&(deflated.len() as u32).to_le_bytes());
    }
    for (_, deflated) in &frames {
        stored.extend_from_slice(deflated);
    }
    stored
}

/// One frame deflated ([`compress_plan`], [`compress_part`]).
fn deflate(frame: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = flate2::write::DeflateEncoder::new(
        Vec::with_capacity(frame.len() / 8),
        flate2::Compression::fast(),
    );
    encoder
        .write_all(frame)
        .and_then(|()| encoder.finish())
        .expect("deflating into memory cannot fail")
}

/// The plan of a stored PLAN section: inflated when [`compress_plan`]
/// wrote it (frames on threads), else as it is. A malformed frame table, a
/// frame that does not inflate to its recorded size, or bytes left over
/// are errors (the section's checksum already passed: a bug or a crafted
/// file, never a torn write).
/// Graph-only preflight, before any frame vector or decompressed allocation.
/// Ordinary build/Index decoding retains its existing contract.
pub(crate) fn check_compressed_bounds(stored: &[u8], max_raw: usize) -> Result<(), String> {
    let malformed = || "malformed or oversized configured graph frames".to_owned();
    let rest = stored
        .strip_prefix(COMPRESSED_MAGIC.as_slice())
        .ok_or_else(malformed)?;
    let count =
        u32::from_le_bytes(rest.get(..4).ok_or_else(malformed)?.try_into().unwrap()) as usize;
    // Graph writers emit full 4 MiB frames, except the final one. This also
    // bounds table allocations independently of the claimed aggregate size.
    if count > max_raw.div_ceil(FRAME) {
        return Err(malformed());
    }
    let end = count
        .checked_mul(8)
        .and_then(|n| n.checked_add(4))
        .ok_or_else(malformed)?;
    let table = rest.get(4..end).ok_or_else(malformed)?;
    let mut offset = end;
    let mut total = 0usize;
    for (index, entry) in table.as_chunks::<8>().0.iter().enumerate() {
        let raw = u32::from_le_bytes(entry[..4].try_into().unwrap()) as usize;
        let len = u32::from_le_bytes(entry[4..].try_into().unwrap()) as usize;
        if raw == 0 || raw > FRAME || (index + 1 < count && raw != FRAME) {
            return Err(malformed());
        }
        total = total
            .checked_add(raw)
            .filter(|n| *n <= max_raw)
            .ok_or_else(malformed)?;
        offset = offset
            .checked_add(len)
            .filter(|n| *n <= rest.len())
            .ok_or_else(malformed)?;
    }
    if offset != rest.len() {
        return Err(malformed());
    }
    Ok(())
}

pub(crate) fn decompress_plan_bounded(stored: Vec<u8>, max_raw: usize) -> Result<Vec<u8>, String> {
    check_compressed_bounds(&stored, max_raw)?;
    decompress_plan(stored)
}

pub fn decompress_plan(stored: Vec<u8>) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let Some(rest) = stored.strip_prefix(COMPRESSED_MAGIC.as_slice()) else {
        return Ok(stored);
    };
    let malformed = || "malformed compressed plan".to_owned();
    let count = u32::from_le_bytes(rest.get(..4).ok_or_else(malformed)?.try_into().unwrap());
    let count = usize::try_from(count).map_err(|_| malformed())?;
    let table = rest
        .get(4..4 + count.checked_mul(8).ok_or_else(malformed)?)
        .ok_or_else(malformed)?;
    let mut frames = Vec::with_capacity(count);
    let mut offset = 4 + table.len();
    let mut total = 0usize;
    for entry in table.as_chunks::<8>().0 {
        let raw = u32::from_le_bytes(entry[..4].try_into().unwrap()) as usize;
        let len = u32::from_le_bytes(entry[4..].try_into().unwrap()) as usize;
        if raw > FRAME {
            return Err(malformed());
        }
        let deflated = rest.get(offset..offset + len).ok_or_else(malformed)?;
        frames.push((deflated, raw));
        offset += len;
        total += raw;
    }
    if offset != rest.len() {
        return Err(malformed());
    }
    // Each frame inflates in place into its slice of the plan (no second
    // copy of a plan of hundreds of MB).
    let mut plan = vec![0u8; total];
    let mut jobs: Vec<(&[u8], &mut [u8])> = Vec::with_capacity(frames.len());
    let mut remaining = plan.as_mut_slice();
    for (deflated, raw) in &frames {
        let (slice, rest) = remaining.split_at_mut(*raw);
        jobs.push((deflated, slice));
        remaining = rest;
    }
    let inflate = |(deflated, slice): (&[u8], &mut [u8])| -> Result<(), String> {
        let mut decoder = flate2::read::DeflateDecoder::new(deflated);
        decoder
            .read_exact(slice)
            .map_err(|error| format!("a compressed plan frame does not inflate: {error}"))?;
        let mut extra = [0u8; 1];
        match decoder.read(&mut extra) {
            Ok(0) => Ok(()),
            Ok(_) => Err("a compressed plan frame inflates to more than its size".to_owned()),
            Err(error) => Err(format!("a compressed plan frame does not inflate: {error}")),
        }
    };
    let threads = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .clamp(1, FRAME_THREADS)
        .min(jobs.len());
    if threads <= 1 {
        for job in jobs {
            inflate(job)?;
        }
        return Ok(plan);
    }
    let queue = std::sync::Mutex::new(jobs);
    let failure = std::sync::Mutex::new(None::<String>);
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let job = queue
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .pop();
                    let Some(job) = job else {
                        break;
                    };
                    if let Err(error) = inflate(job) {
                        failure
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .get_or_insert(error);
                    }
                }
            });
        }
    });
    match failure
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        Some(error) => Err(error),
        None => Ok(plan),
    }
}

// ---- INPUTS ----------------------------------------------------------------

/// What a hit validates and replays (the INPUTS section, decoded).
#[derive(Clone, Debug, Default)]
pub struct Inputs {
    /// The observations, environment digests and repository trees the
    /// command's policy validates (crate::snapshot::validate::Policy).
    pub recording: Recording,
    pub replay: Vec<ReplayRecord>,
    pub tables: Option<PinnedTables>,
    /// Planning computed the workspace status (its command ran; under
    /// `--nobuild` for a consumer of `ctx.info_file`, without placeholders
    /// in the plan): a hit runs the command too, quietly, and misses when it
    /// writes to stderr (L2-W5f review F2).
    pub workspace_status: bool,
}

/// Interns paths into the front-coded path table.
#[derive(Default)]
struct PathTable<'a> {
    paths: BTreeSet<&'a [u8]>,
}

impl<'a> PathTable<'a> {
    fn add(&mut self, path: &'a std::path::Path) {
        self.paths.insert(path.as_os_str().as_bytes());
    }

    /// The front-coded table and the index of every path.
    #[allow(clippy::type_complexity)]
    fn finish(self) -> (Vec<(u16, Bytes<'a>)>, HashMap<&'a [u8], u32>) {
        let mut coded = Vec::with_capacity(self.paths.len());
        let mut index = HashMap::with_capacity(self.paths.len());
        let mut previous: &[u8] = &[];
        for (position, path) in self.paths.into_iter().enumerate() {
            let shared = previous
                .iter()
                .zip(path)
                .take_while(|(a, b)| a == b)
                .count()
                .min(usize::from(u16::MAX));
            coded.push((shared as u16, Bytes(&path[shared..])));
            index.insert(path, position as u32);
            previous = path;
        }
        (coded, index)
    }
}

fn decode_paths(coded: &[(u16, Bytes<'_>)]) -> Result<Vec<PathBuf>, String> {
    let mut paths: Vec<PathBuf> = Vec::with_capacity(coded.len());
    let mut previous: Vec<u8> = Vec::new();
    for (shared, suffix) in coded {
        let shared = usize::from(*shared);
        if shared > previous.len() {
            return Err("malformed path table".to_owned());
        }
        previous.truncate(shared);
        previous.extend_from_slice(suffix.0);
        paths.push(path_of(&previous));
    }
    Ok(paths)
}

/// Encode the INPUTS section: `recording` as the policy selected it, the
/// replay records and their pinned tables.
pub fn encode_inputs(
    recording: &Recording,
    replay: &[ReplayRecord],
    tables: Option<&PinnedTables>,
    workspace_status: bool,
) -> Vec<u8> {
    let mut table = PathTable::default();
    for (path, _) in &recording.observations {
        table.add(path);
    }
    for tree in &recording.repositories {
        table.add(&tree.root);
        if let Some(manifest) = &tree.manifest {
            table.add(manifest);
        }
        for entry in &tree.entries {
            table.add(&entry.path);
        }
    }
    let (paths, index) = table.finish();
    let at = |path: &std::path::Path| index[path.as_os_str().as_bytes()];
    let observations = recording
        .observations
        .iter()
        .map(|(path, observation)| ObservationWire {
            path: at(path),
            op: match &observation.op {
                Op::Read => OpWire::Read,
                Op::Stat => OpWire::Stat,
                Op::Lstat => OpWire::Lstat,
                Op::ReadDir(projection) => OpWire::ReadDir(match projection {
                    Projection::All => ProjectionWire::All,
                    Projection::Directories => ProjectionWire::Directories,
                    Projection::Glob { pattern, relative } => {
                        ProjectionWire::Glob { pattern, relative }
                    }
                }),
                Op::Canon => OpWire::Canon,
                Op::ReadLink => OpWire::ReadLink,
            },
            outcome: match &observation.outcome {
                Outcome::Error(errno) => OutcomeWire::Error(*errno),
                Outcome::Read { sha256, key, racy } => OutcomeWire::Read {
                    sha256: *sha256,
                    key: key.into(),
                    racy: *racy,
                },
                Outcome::Kind(kind) => OutcomeWire::Kind(kind_code(*kind)),
                Outcome::Link(target) => OutcomeWire::Link(Bytes(target.as_os_str().as_bytes())),
                Outcome::Listing {
                    digest,
                    entries,
                    key,
                    racy,
                } => OutcomeWire::Listing {
                    digest: *digest,
                    entries: *entries,
                    key: key.into(),
                    racy: *racy,
                },
                Outcome::Path(path) => OutcomeWire::Path(Bytes(path.as_os_str().as_bytes())),
            },
        })
        .collect();
    let env = recording
        .env
        .iter()
        .map(|(name, observation)| EnvWire {
            name: Bytes(name.as_bytes()),
            digest: observation.digest,
        })
        .collect();
    let trees = recording
        .repositories
        .iter()
        .map(|tree| TreeWire {
            kind: TREE_KINDS
                .iter()
                .position(|kind| *kind == tree.kind)
                .unwrap_or(0) as u8,
            name: &tree.name,
            root: at(&tree.root),
            manifest: tree.manifest.as_deref().map(at),
            entries: tree
                .entries
                .iter()
                .map(|entry| EntryWire {
                    path: at(&entry.path),
                    state: entry
                        .state
                        .as_ref()
                        .map(|(kind, key)| (kind_code(*kind), key.into()))
                        .map_err(|errno| *errno),
                    racy: entry.racy,
                })
                .collect(),
        })
        .collect();
    let replay = replay
        .iter()
        .map(|record| ReplayWire {
            canonical: &record.canonical,
            original_name: &record.original_name,
            definition: match &record.definition {
                DefinitionSource::Direct { label, symbol } => DefinitionWire::Direct {
                    label: label.clone(),
                    symbol,
                },
                DefinitionSource::Ordinary { rule_id } => DefinitionWire::Ordinary { rule_id },
            },
            identity: &record.identity,
            definition_label: record.definition_label.clone(),
            attributes: record.attributes.clone(),
            root: Bytes(record.root.as_os_str().as_bytes()),
            inputs_digest: record.execution.inputs_digest,
            snapshot_digest: record.execution.snapshot_digest,
            execution_identity: &record.execution.identity,
            output: Bytes(&record.output),
            environ_read: record.environ_read,
            facilities: record.facilities.clone(),
            elapsed_ns: u64::try_from(record.elapsed.as_nanos()).unwrap_or(u64::MAX),
        })
        .collect();
    let tables = tables.map(|tables| TablesWire {
        main_root: Bytes(tables.main_root.as_os_str().as_bytes()),
        source_cache: Bytes(tables.source_cache.as_os_str().as_bytes()),
        roots: tables
            .roots
            .iter()
            .map(|(name, root)| {
                (
                    name.as_str(),
                    Bytes(root.path.as_os_str().as_bytes()),
                    root.generated,
                )
            })
            .collect(),
        mappings: tables
            .mappings
            .iter()
            .map(|(name, mapping)| {
                (
                    name.as_str(),
                    mapping
                        .mapping
                        .iter()
                        .map(|(apparent, canonical)| (apparent.as_str(), canonical.as_str()))
                        .collect(),
                    mapping.isolated.iter().map(String::as_str).collect(),
                )
            })
            .collect(),
    });
    postcard::to_stdvec(&InputsWire {
        window_start_ns: recording.window_start_ns,
        paths,
        observations,
        env,
        trees,
        replay,
        tables,
        workspace_status,
    })
    .expect("the inputs encode")
}

/// Decode an INPUTS section.
pub fn decode_inputs(bytes: &[u8]) -> Result<Inputs, String> {
    let wire: InputsWire<'_> =
        postcard::from_bytes(bytes).map_err(|error| format!("INPUTS: {error}"))?;
    let paths = decode_paths(&wire.paths)?;
    let path = |index: u32| -> Result<PathBuf, String> {
        paths
            .get(index as usize)
            .cloned()
            .ok_or_else(|| format!("INPUTS: path index {index} out of range"))
    };
    let mut observations = Vec::with_capacity(wire.observations.len());
    for observation in &wire.observations {
        let op = match &observation.op {
            OpWire::Read => Op::Read,
            OpWire::Stat => Op::Stat,
            OpWire::Lstat => Op::Lstat,
            OpWire::ReadDir(projection) => Op::ReadDir(match projection {
                ProjectionWire::All => Projection::All,
                ProjectionWire::Directories => Projection::Directories,
                ProjectionWire::Glob { pattern, relative } => Projection::Glob {
                    pattern: (*pattern).to_owned(),
                    relative: (*relative).to_owned(),
                },
            }),
            OpWire::Canon => Op::Canon,
            OpWire::ReadLink => Op::ReadLink,
        };
        let outcome = match &observation.outcome {
            OutcomeWire::Error(errno) => Outcome::Error(*errno),
            OutcomeWire::Read { sha256, key, racy } => Outcome::Read {
                sha256: *sha256,
                key: (*key).into(),
                racy: *racy,
            },
            OutcomeWire::Kind(code) => Outcome::Kind(kind_of(*code)?),
            OutcomeWire::Link(target) => Outcome::Link(path_of(target.0)),
            OutcomeWire::Listing {
                digest,
                entries,
                key,
                racy,
            } => Outcome::Listing {
                digest: *digest,
                entries: *entries,
                key: (*key).into(),
                racy: *racy,
            },
            OutcomeWire::Path(resolved) => Outcome::Path(path_of(resolved.0)),
        };
        observations.push((path(observation.path)?, Observation { op, outcome }));
    }
    let env = wire
        .env
        .iter()
        .map(|env| {
            (
                OsString::from_vec(env.name.0.to_vec()),
                EnvObservation { digest: env.digest },
            )
        })
        .collect();
    let mut repositories = Vec::with_capacity(wire.trees.len());
    for tree in &wire.trees {
        let mut entries = Vec::with_capacity(tree.entries.len());
        for entry in &tree.entries {
            entries.push(IntegrityEntry {
                path: path(entry.path)?,
                state: match &entry.state {
                    Ok((kind, key)) => Ok((kind_of(*kind)?, (*key).into())),
                    Err(errno) => Err(*errno),
                },
                racy: entry.racy,
            });
        }
        repositories.push(RepositoryRecord {
            kind: TREE_KINDS
                .get(usize::from(tree.kind))
                .copied()
                .ok_or_else(|| format!("INPUTS: unknown tree kind {}", tree.kind))?,
            name: tree.name.to_owned(),
            root: path(tree.root)?,
            manifest: tree.manifest.map(path).transpose()?,
            entries,
        });
    }
    let replay = wire
        .replay
        .into_iter()
        .map(|record| ReplayRecord {
            canonical: record.canonical.to_owned(),
            original_name: record.original_name.to_owned(),
            definition: match record.definition {
                DefinitionWire::Direct { label, symbol } => DefinitionSource::Direct {
                    label,
                    symbol: symbol.to_owned(),
                },
                DefinitionWire::Ordinary { rule_id } => DefinitionSource::Ordinary {
                    rule_id: rule_id.to_owned(),
                },
            },
            identity: record.identity.to_owned(),
            definition_label: record.definition_label,
            attributes: record.attributes,
            root: path_of(record.root.0),
            execution: crate::repository_store::LocalExecution {
                inputs_digest: record.inputs_digest,
                snapshot_digest: record.snapshot_digest,
                identity: record.execution_identity.to_owned(),
            },
            output: record.output.0.to_vec(),
            environ_read: record.environ_read,
            facilities: record
                .facilities
                .iter()
                .map(|name| {
                    FACILITIES
                        .iter()
                        .copied()
                        .find(|known| known == name)
                        .unwrap_or("other")
                })
                .collect(),
            elapsed: Duration::from_nanos(record.elapsed_ns),
        })
        .collect();
    let tables = wire.tables.map(|tables| PinnedTables {
        main_root: path_of(tables.main_root.0),
        source_cache: path_of(tables.source_cache.0),
        roots: tables
            .roots
            .iter()
            .map(|(name, root, generated)| {
                (
                    (*name).to_owned(),
                    PinnedRoot {
                        path: path_of(root.0),
                        generated: *generated,
                    },
                )
            })
            .collect(),
        mappings: tables
            .mappings
            .iter()
            .map(|(name, mapping, isolated)| {
                (
                    (*name).to_owned(),
                    PinnedMapping {
                        mapping: mapping
                            .iter()
                            .map(|(apparent, canonical)| {
                                ((*apparent).to_owned(), (*canonical).to_owned())
                            })
                            .collect(),
                        isolated: isolated.iter().map(|name| (*name).to_owned()).collect(),
                    },
                )
            })
            .collect(),
    });
    Ok(Inputs {
        recording: Recording {
            window_start_ns: wire.window_start_ns,
            window: None,
            observations,
            env,
            repositories,
            crossed: Vec::new(),
            taints: BTreeMap::new(),
            plan_registered: None,
        },
        replay,
        tables,
        workspace_status: wire.workspace_status,
    })
}

// ---- EVENTS and STATS ------------------------------------------------------

pub fn encode_events(window_stderr: &[u8]) -> Vec<u8> {
    postcard::to_stdvec(&EventsWire {
        window_stderr: Bytes(window_stderr),
    })
    .expect("the events encode")
}

/// The window's stderr bytes.
pub fn decode_events(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let wire: EventsWire<'_> =
        postcard::from_bytes(bytes).map_err(|error| format!("EVENTS: {error}"))?;
    Ok(wire.window_stderr.0.to_vec())
}

pub fn encode_stats(stats: &Stats) -> Vec<u8> {
    postcard::to_stdvec(stats).expect("the stats encode")
}

pub fn decode_stats(bytes: &[u8]) -> Result<Stats, String> {
    postcard::from_bytes(bytes).map_err(|error| format!("STATS: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observed_fs::{FileKind, IntegrityEntry, RepositoryRecord};
    use std::path::Path;

    fn raw(bytes: &[u8]) -> PathBuf {
        path_of(bytes)
    }

    fn key(seed: i64) -> StatKey {
        StatKey {
            dev: 1,
            ino: seed as u64,
            size: 3,
            mtime_ns: seed,
            ctime_ns: -seed,
        }
    }

    pub(crate) fn sample() -> (Recording, Vec<ReplayRecord>, PinnedTables) {
        let observation = |op: Op, outcome: Outcome| Observation { op, outcome };
        let recording = Recording {
            window_start_ns: 42,
            window: None,
            observations: vec![
                (
                    raw(b"/ws/BUILD"),
                    observation(
                        Op::Read,
                        Outcome::Read {
                            sha256: [9; 32],
                            key: key(1),
                            racy: true,
                        },
                    ),
                ),
                (
                    raw(b"/ws/BUILD"),
                    observation(Op::Lstat, Outcome::Link(raw(b"../x\xff"))),
                ),
                (
                    raw(b"/ws/pkg"),
                    observation(
                        Op::ReadDir(Projection::Glob {
                            pattern: "**/*.go".into(),
                            relative: "a".into(),
                        }),
                        Outcome::Listing {
                            digest: [3; 32],
                            entries: 7,
                            key: key(2),
                            racy: false,
                        },
                    ),
                ),
                (
                    raw(b"/ws/pkg"),
                    observation(Op::ReadDir(Projection::All), Outcome::Error(13)),
                ),
                (
                    raw(b"/ws/p\xfe"),
                    observation(Op::Stat, Outcome::Kind(FileKind::Dir)),
                ),
                (
                    raw(b"/ws/q"),
                    observation(Op::Canon, Outcome::Path(raw(b"/real/q"))),
                ),
                (
                    raw(b"/ws/q"),
                    observation(Op::ReadDir(Projection::Directories), Outcome::Error(2)),
                ),
                (
                    raw(b"/ws/r"),
                    observation(Op::ReadLink, Outcome::Path(raw(b"t"))),
                ),
            ],
            env: vec![
                (
                    OsString::from("PATH"),
                    EnvObservation {
                        digest: Some([5; 32]),
                    },
                ),
                (
                    OsString::from_vec(b"X\xff".to_vec()),
                    EnvObservation { digest: None },
                ),
            ],
            repositories: vec![RepositoryRecord {
                kind: "git",
                name: "remote@x".into(),
                root: raw(b"/c/git/trees/ab"),
                manifest: Some(raw(b"/c/git/manifests/ab.json")),
                entries: vec![
                    IntegrityEntry {
                        path: raw(b"/c/git/manifests/ab.json"),
                        state: Ok((FileKind::File, key(3))),
                        racy: false,
                    },
                    IntegrityEntry {
                        path: raw(b"/c/git/trees/ab"),
                        state: Ok((FileKind::Dir, key(4))),
                        racy: true,
                    },
                    IntegrityEntry {
                        path: raw(b"/c/git/trees/ab/gone"),
                        state: Err(2),
                        racy: false,
                    },
                ],
            }],
            crossed: Vec::new(),
            taints: BTreeMap::new(),
            plan_registered: None,
        };
        let record = ReplayRecord {
            canonical: "rules+ext+local".into(),
            original_name: "local".into(),
            definition: DefinitionSource::Direct {
                label: Label::parse_canonical("@@rules+//x:defs.bzl").unwrap(),
                symbol: "rule".into(),
            },
            identity: "@@rules+//x:defs.bzl%rule".into(),
            definition_label: Label::parse_canonical("@@rules+//x:defs.bzl").unwrap(),
            attributes: BTreeMap::from([
                ("name".to_owned(), AttrValue::String("local".into())),
                ("n".to_owned(), AttrValue::Ints(vec![1, -2])),
            ]),
            root: raw(b"/c/direct/trees/cd"),
            execution: crate::repository_store::LocalExecution {
                inputs_digest: [1; 32],
                snapshot_digest: [2; 32],
                identity: "cd".into(),
            },
            output: Vec::new(),
            environ_read: true,
            facilities: vec!["execute", "which"],
            elapsed: Duration::from_micros(1234),
        };
        let other = ReplayRecord {
            canonical: "ext+config".into(),
            definition: DefinitionSource::Ordinary {
                rule_id: "@@rules+//y:z.bzl%_config".into(),
            },
            output: b"printed\n".to_vec(),
            facilities: Vec::new(),
            environ_read: false,
            ..record.clone()
        };
        let tables = PinnedTables {
            main_root: raw(b"/ws"),
            source_cache: raw(b"/c"),
            roots: BTreeMap::from([(
                "rules+".to_owned(),
                PinnedRoot {
                    path: raw(b"/c/trees/ef"),
                    generated: false,
                },
            )]),
            mappings: BTreeMap::from([(
                "".to_owned(),
                PinnedMapping {
                    mapping: BTreeMap::from([("rules".to_owned(), "rules+".to_owned())]),
                    isolated: BTreeSet::from(["iso".to_owned()]),
                },
            )]),
        };
        (recording, vec![record, other], tables)
    }

    #[test]
    fn inputs_round_trip_byte_for_byte() {
        let (recording, records, tables) = sample();
        let bytes = encode_inputs(&recording, &records, Some(&tables), true);
        assert_eq!(
            encode_inputs(&recording, &records, Some(&tables), true),
            bytes,
            "deterministic"
        );
        let decoded = decode_inputs(&bytes).unwrap();
        assert_eq!(decoded.recording.observations, recording.observations);
        assert_eq!(decoded.recording.env, recording.env);
        assert_eq!(decoded.recording.repositories, recording.repositories);
        assert_eq!(decoded.recording.window_start_ns, 42);
        assert_eq!(decoded.tables.as_ref(), Some(&tables));
        assert_eq!(decoded.replay.len(), 2);
        for (decoded, record) in decoded.replay.iter().zip(&records) {
            assert_eq!(decoded.canonical, record.canonical);
            assert_eq!(decoded.definition, record.definition);
            assert_eq!(decoded.identity, record.identity);
            assert_eq!(decoded.definition_label, record.definition_label);
            assert_eq!(decoded.attributes, record.attributes);
            assert_eq!(decoded.root, record.root);
            assert_eq!(decoded.execution, record.execution);
            assert_eq!(decoded.output, record.output);
            assert_eq!(decoded.facilities, record.facilities);
            assert_eq!(decoded.elapsed, record.elapsed);
        }
        assert!(decoded.workspace_status);
        let again = encode_inputs(
            &decoded.recording,
            &decoded.replay,
            decoded.tables.as_ref(),
            decoded.workspace_status,
        );
        assert_eq!(again, bytes, "re-encoded");
        // Damage never panics.
        for cut in [0, 1, bytes.len() / 2, bytes.len() - 1] {
            let _ = decode_inputs(&bytes[..cut]);
        }
    }

    #[test]
    fn paths_are_front_coded() {
        let mut table = PathTable::default();
        let paths = [
            Path::new("/a/b/c"),
            Path::new("/a/b/d"),
            Path::new("/a"),
            Path::new("/z"),
        ];
        for path in paths {
            table.add(path);
        }
        let (coded, index) = table.finish();
        let shared: Vec<u16> = coded.iter().map(|(shared, _)| *shared).collect();
        assert_eq!(shared, [0, 2, 5, 1]);
        assert_eq!(index[b"/a/b/d".as_slice()], 2);
        let decoded = decode_paths(&coded).unwrap();
        assert_eq!(decoded, ["/a", "/a/b/c", "/a/b/d", "/z"].map(PathBuf::from));
        assert!(decode_paths(&[(3, Bytes(b"x"))]).is_err());
    }

    #[test]
    fn headers_round_trip_and_damage_is_refused() {
        let key = Key {
            g: [4; 32],
            binary: [5; 32],
        };
        let sections: [(Section, &[u8]); 3] = [
            (Section::Inputs, b"inputs"),
            (Section::Events, b""),
            (Section::Plan, b"plan bytes"),
        ];
        let header = Header::new(&key, 77, &sections);
        let bytes = header.encode();
        let total = (HEADER_LEN + 16) as u64;
        assert_eq!(Header::decode(&bytes, total).unwrap(), header);
        let plan = header.slot(Section::Plan).unwrap();
        assert_eq!(plan.offset, HEADER_LEN as u64 + 6);
        assert_eq!(plan.checksum, checksum(b"plan bytes"));
        assert!(
            Header::decode(&bytes, total - 1)
                .unwrap_err()
                .contains("outside")
        );
        let mut damaged = bytes;
        damaged[100] ^= 1;
        assert!(
            Header::decode(&damaged, total)
                .unwrap_err()
                .contains("checksum")
        );
        assert!(Header::decode(&bytes[..10], total).is_err());
        let mut magic = bytes;
        magic[0] = b'X';
        assert!(Header::decode(&magic, total).unwrap_err().contains("magic"));
    }

    #[test]
    fn events_and_stats_round_trip() {
        let events = encode_events(b"WARNING: x\n\xff");
        assert_eq!(decode_events(&events).unwrap(), b"WARNING: x\n\xff");
        let stats = Stats {
            observations: 3,
            plan_bytes: 1 << 40,
            full_validation: true,
            ..Stats::default()
        };
        assert_eq!(decode_stats(&encode_stats(&stats)).unwrap(), stats);
    }

    /// P3: a plan of several frames compresses deterministically and
    /// inflates back; an uncompressed plan reads as it is; damage is an
    /// error.
    #[test]
    fn plans_compress_in_frames_and_inflate_back() {
        // Repetitive like a plan, not trivially so: 2.5 frames.
        let plan: Vec<u8> = (0..(FRAME * 5 / 2))
            .map(|index| ((index / 7) % 251) as u8 ^ ((index % 13) as u8))
            .collect();
        let stored = compress_plan(&plan);
        assert!(stored.starts_with(COMPRESSED_MAGIC) && stored.len() < plan.len() / 4);
        assert_eq!(u32::from_le_bytes(stored[8..12].try_into().unwrap()), 3);
        assert_eq!(compress_plan(&plan), stored);
        // MEM4: one part compresses like the whole plan; parts cut
        // anywhere (empty ones too) inflate back to their concatenation.
        assert_eq!(
            assemble_compressed(vec![compress_part(plan.clone())]),
            stored
        );
        let cut = [0, 1, 1, FRAME - 1, FRAME + 7, plan.len()];
        let parts: Vec<_> = cut
            .windows(2)
            .map(|pair| compress_part(plan[pair[0]..pair[1]].to_vec()))
            .collect();
        assert_eq!(decompress_plan(assemble_compressed(parts)).unwrap(), plan);
        assert_eq!(assemble_compressed(Vec::new()), compress_plan(b""));
        assert_eq!(decompress_plan(stored.clone()).unwrap(), plan);
        assert_eq!(decompress_plan(compress_plan(b"")).unwrap(), b"");
        assert_eq!(
            decompress_plan(b"BZRSPLN1 as encoded".to_vec()).unwrap(),
            b"BZRSPLN1 as encoded"
        );
        // A frame's recorded size, the frame table, the frames' bytes.
        let mut wrong_size = stored.clone();
        wrong_size[12] ^= 1;
        assert!(decompress_plan(wrong_size).is_err());
        let mut truncated = stored.clone();
        truncated.pop();
        assert!(decompress_plan(truncated).is_err());
        let mut extra = stored.clone();
        extra.push(0);
        assert!(decompress_plan(extra).is_err());
        let mut damaged = stored.clone();
        let last = damaged.len() - 40;
        damaged[last] ^= 0xff;
        assert!(decompress_plan(damaged).is_err_and(|error| error.contains("inflate")));
        assert!(decompress_plan(COMPRESSED_MAGIC.to_vec()).is_err());
    }
}
