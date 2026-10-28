//! Two fixed graph slots per original analysis key, inside the ordinary store.
//! No revision identity, mutable index, independent GC budget or resident memo.
use crate::observed_fs::{Outcome, StatKey};
use crate::snapshot::{format, key::Key, store};
use format::Section;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

// Header-only evidence: the 265k-target graph has 404,498,195 postcard bytes,
// 24,530,264 stored PLAN bytes, 7,961,305 INPUTS and 88,297 EVENTS bytes.
pub(super) const MAX_RAW: usize = 768 << 20;
const MAX_ENTRY: u64 = 256 << 20;
const MAX_INPUTS: u64 = 64 << 20;
const MAX_EVENTS: u64 = 8 << 20;
pub(super) const MAX_PLAN: u64 = 192 << 20;
const KINDS: [Section; 4] = [
    Section::Inputs,
    Section::Events,
    Section::Plan,
    Section::Stats,
];

fn limit(kind: Section) -> u64 {
    match kind {
        Section::Inputs => MAX_INPUTS,
        Section::Events => MAX_EVENTS,
        Section::Plan => MAX_PLAN,
        Section::Stats => 32,
        // Future shared format sections (e.g. build's incremental index) are
        // not part of a configured graph.
        #[allow(unreachable_patterns)]
        _ => 0,
    }
}

fn path(cache: &Path, key: &Key, slot: usize) -> PathBuf {
    store::directory(cache).join(format!("{}-cq{slot}.snap", key.short()))
}

fn open(cache: &Path, key: &Key, slot: usize) -> Result<Option<store::Reader>, String> {
    let Some(reader) = store::Reader::open_path_bounded(path(cache, key, slot), key, MAX_ENTRY)?
    else {
        return Ok(None);
    };
    // Exact, contiguous graph sections; reject duplicates/overlap/unknown
    // sections, and every section limit, before the first section allocation.
    if reader.header.slots.len() != KINDS.len() {
        return Err("invalid configured graph sections".into());
    }
    let mut offset = format::HEADER_LEN as u64;
    for (slot, kind) in reader.header.slots.iter().zip(KINDS) {
        if slot.kind != kind as u32
            || slot.offset != offset
            || slot.len > limit(kind)
            || (kind == Section::Stats && slot.len != 32)
        {
            return Err("invalid or oversized configured graph section".into());
        }
        offset = offset
            .checked_add(slot.len)
            .ok_or("configured graph size overflow")?;
    }
    Ok(Some(reader))
}

/// MRU first (mtime), then newest generation, then slot name. Both open FDs
/// pin their generations; only headers are resident before sequential probing.
pub(super) fn candidates(cache: &Path, key: &Key) -> Vec<Result<store::Reader, String>> {
    let mut readers = Vec::with_capacity(2);
    let mut errors = Vec::new();
    for slot in 0..2 {
        match open(cache, key, slot) {
            Ok(Some(reader)) => readers.push(reader),
            Ok(None) => {}
            Err(error) => errors.push(Err(error)),
        }
    }
    readers.sort_by_key(|reader| {
        std::cmp::Reverse((
            reader.modified(),
            reader.header.created_ns,
            reader.path.clone(),
        ))
    });
    readers.into_iter().map(Ok).chain(errors).collect()
}

pub(super) fn section(reader: &store::Reader, kind: Section) -> Result<Vec<u8>, String> {
    let bytes = reader.stored_section_bounded(kind, limit(kind))?;
    if kind == Section::Plan {
        format::decompress_plan_bounded(bytes, MAX_RAW)
    } else {
        Ok(bytes)
    }
}

/// A content version includes all observations, replay/publication identities,
/// diagnostics and graph bytes. Stat fast-path hints/window times are not
/// version identity: a checkout or verification can refresh those same inputs.
/// This is only a write-dedup fingerprint; hits always run the full validator.
fn version(inputs: &[u8], events: &[u8], plan: &[u8]) -> Result<[u8; 32], String> {
    let mut inputs = format::decode_inputs(inputs)?;
    inputs.recording.window_start_ns = 0;
    for (_, observation) in &mut inputs.recording.observations {
        match &mut observation.outcome {
            Outcome::Read { key, racy, .. } | Outcome::Listing { key, racy, .. } => {
                *key = StatKey {
                    dev: 0,
                    ino: 0,
                    size: 0,
                    mtime_ns: 0,
                    ctime_ns: 0,
                };
                *racy = false;
            }
            _ => {}
        }
    }
    for record in &mut inputs.replay {
        record.elapsed = Duration::ZERO;
    }
    let normalized = format::encode_inputs(
        &inputs.recording,
        &inputs.replay,
        inputs.tables.as_ref(),
        inputs.workspace_status,
    );
    let mut digest = blake3::Hasher::new();
    digest.update(b"cquery-history-version-v1");
    for bytes in [normalized.as_slice(), events, plan] {
        digest.update(blake3::hash(bytes).as_bytes());
    }
    Ok(*digest.finalize().as_bytes())
}

/// One fixed lock per store, never unlinked (no lock-inode split or per-G
/// accumulation). Compression/staging happen before this bounded wait. A
/// contended save is optional; readers never wait for this lock.
fn lock(cache: &Path) -> Result<File, String> {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(store::root(cache).join("cquery-history.lock"))
        .map_err(|e| e.to_string())?;
    let start = std::time::Instant::now();
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(file),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && start.elapsed() < Duration::from_secs(2) =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(format!("configured graph publication lock: {error}")),
        }
    }
}

fn current(reader: &store::Reader) -> bool {
    // snapshot-unrecorded: cache generation identity, never an analysis input.
    fs::symlink_metadata(&reader.path).is_ok_and(|m| (m.dev(), m.ino()) == reader.identity())
}

/// The reader is borrowed until after removal, preventing inode reuse. All
/// graph replacements/removals take the same lock. Ordinary GC can only unlink.
pub(super) fn remove(cache: &Path, reader: &store::Reader) -> Result<(), String> {
    let _lock = lock(cache)?;
    if current(reader) {
        fs::remove_file(&reader.path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "cquery_history/tests.rs"]
mod tests;

pub(super) fn save(
    cache: &Path,
    key: &Key,
    inputs: Vec<u8>,
    events: Vec<u8>,
    plan: Vec<u8>,
    verified: Option<&store::Reader>,
) -> Result<(), String> {
    let sections = [
        (Section::Inputs, inputs),
        (Section::Events, events),
        (Section::Plan, plan),
    ];
    let mut total = format::HEADER_LEN as u64 + 32;
    for (kind, bytes) in &sections {
        if bytes.len() as u64 > limit(*kind) {
            return Err(format!(
                "{} exceeds configured graph size limit",
                kind.name()
            ));
        }
        total = total
            .checked_add(bytes.len() as u64)
            .ok_or("configured graph size overflow")?;
    }
    if total > MAX_ENTRY {
        return Err("configured graph entry exceeds size limit".into());
    }
    format::check_compressed_bounds(&sections[2].1, MAX_RAW)?;
    let version = version(&sections[0].1, &sections[1].1, &sections[2].1)?;
    fs::create_dir_all(store::directory(cache)).map_err(|e| e.to_string())?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".tmp-cquery-")
        .tempfile_in(store::directory(cache))
        .map_err(|e| e.to_string())?;
    let mut borrowed: Vec<_> = sections
        .iter()
        .map(|(kind, bytes)| (*kind, bytes.as_slice()))
        .collect();
    borrowed.push((Section::Stats, &version));
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let header = format::Header::new(
        key,
        i64::try_from(now.as_nanos()).unwrap_or(i64::MAX),
        &borrowed,
    );
    temporary
        .write_all(&header.encode())
        .map_err(|e| e.to_string())?;
    for (_, bytes) in &borrowed {
        temporary.write_all(bytes).map_err(|e| e.to_string())?;
    }
    drop(borrowed);
    drop(sections);

    let _lock = lock(cache)?;
    if verified.is_some_and(|reader| !current(reader)) {
        return Err(
            "verified configured graph generation was removed or replaced; not refreshed".into(),
        );
    }
    // Re-open under the publication lock: concurrent fresh analyses of A
    // replace A, never fill both slots with A and evict B.
    let slots = [
        open(cache, key, 0).ok().flatten(),
        open(cache, key, 1).ok().flatten(),
    ];
    let duplicate = slots.iter().position(|reader| {
        reader.as_ref().is_some_and(|reader| {
            section(reader, Section::Stats).is_ok_and(|bytes| bytes == version)
        })
    });
    let destination = if let Some(reader) = verified {
        // Verification can observe a changed input whose graph is equal (for
        // example .bazelignore). Do not turn A into a second B when B already
        // exists, nor refresh another generation the verification did not pin.
        if let Some(duplicate) = duplicate.and_then(|slot| slots[slot].as_ref())
            && duplicate.path != reader.path
        {
            duplicate.touch();
            return Ok(());
        }
        reader.path.clone()
    } else {
        let slot = duplicate
            .or_else(|| slots.iter().position(Option::is_none))
            .unwrap_or_else(|| {
                // Evict the LRU; deterministic slot 0 on equal timestamps.
                usize::from(
                    slots[1].as_ref().unwrap().modified() < slots[0].as_ref().unwrap().modified(),
                )
            });
        path(cache, key, slot)
    };
    temporary.persist(destination).map_err(|e| e.to_string())?;
    // Both .snap slots participate in the SAME KEEP and byte budget. As for
    // ordinary saves, staging and the instant before GC can exceed that budget.
    store::collect_garbage(cache);
    Ok(())
}
