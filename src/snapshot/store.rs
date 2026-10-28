//! The snapshot store (PLAN-L2 §1d "Rules for the store"): one file per key
//! G, `<state>/snapshots/v1/<first 16 bytes of G in hex>.snap`, in the
//! binary format of crate::snapshot::format. `<state>` is the workspace's
//! state directory in the cache, `<cache>/worktrees/<name>-<hash>`
//! (crate::worktree::state_directory): every worktree sharing a cache has
//! its own store and its own garbage-collection budget.
//!
//! * **Reading** never locks: open, read and check the header (magic,
//!   format, binary identity, the full G), then read single sections by
//!   offset (`pread`), each checked against its checksum. A file whose
//!   format or binary identity differs is treated as absent; a damaged one
//!   (a torn write, a truncation) as a miss.
//! * **Writing** happens on a background thread while the actions execute:
//!   a temporary file `.tmp-<name>-<pid>-<n>` in the same directory, no
//!   fsync (the checksums catch torn writes). It is renamed into place only
//!   when the command succeeded ([`Pending::commit`]); otherwise removed.
//!   Several writers of the same G all write valid files; the last rename
//!   wins. A reader that already opened a file keeps reading it.
//! * **Garbage collection** after each save: at most [`KEEP`] snapshots and
//!   a size-aware byte budget per store (per worktree, [`budget`]: 10 % of
//!   what the store's file system has available plus the store's own bytes,
//!   between [`BUDGET_FLOOR`] and [`BUDGET_CAP`]), least recently used first
//!   (a hit sets its file's mtime), and temporary files older than an hour.
//! * **Set aside** (L2-W5i): a snapshot a verify-mode mismatch contradicted
//!   is moved to `<state>/snapshots/mismatched/` ([`set_aside`]) instead of
//!   being deleted, so the soak's report can be acted on. Lookups never
//!   look there and the garbage collection never counts it (neither the
//!   [`KEEP`] snapshots nor the byte budget): its own bound is the
//!   [`MISMATCHED_KEEP`] newest files.

use crate::snapshot::format::{self, HEADER_LEN, Header, Section};
use crate::snapshot::key::Key;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

/// Snapshots kept per store (per worktree) at most. (P3: 16 before; with
/// ~/code's large trees saved — rust 331 MB, bworker 344 MB — every first
/// warm run of a survey missed on an eviction.)
pub const KEEP: usize = 64;
/// The byte budget of a store at most ([`budget`]).
pub const BUDGET_CAP: u64 = 4 << 30;
/// The byte budget of a store at least ([`budget`]): one snapshot of the
/// largest size written.
pub const BUDGET_FLOOR: u64 = MAX_BYTES;
/// The byte budget's share, in percent, of the store's file system's
/// available bytes plus the store's own ([`budget`]).
pub const BUDGET_PERCENT: u64 = 10;
/// The largest snapshot written (PLAN-L2 §1b "Size").
pub const MAX_BYTES: u64 = 512 << 20;
/// Snapshots a verify mismatch set aside that are kept ([`set_aside`]).
pub const MISMATCHED_KEEP: usize = 3;
const TEMPORARY_PREFIX: &str = ".tmp-";
const STALE_TEMPORARY: Duration = Duration::from_secs(3600);
const SUFFIX: &str = ".snap";

/// `<state>/snapshots`: the binary identity cache and the versioned store
/// of this process's workspace (crate::worktree::state_directory; without
/// an installed workspace, `<cache>/snapshots`).
pub fn root(cache: &Path) -> PathBuf {
    crate::worktree::state_directory(cache).join("snapshots")
}

/// The directory of this format's snapshot files.
pub fn directory(cache: &Path) -> PathBuf {
    root(cache).join("v1")
}

/// The file of `key`.
pub fn path(cache: &Path, key: &Key) -> PathBuf {
    directory(cache).join(format!("{}{SUFFIX}", key.short()))
}

/// Where [`set_aside`] moves the snapshots a verify mismatch contradicted
/// (outside [`directory`]: lookups and the garbage collection never see
/// them).
pub fn mismatched_directory(cache: &Path) -> PathBuf {
    root(cache).join("mismatched")
}

/// An open snapshot whose header matched the key.
#[derive(Debug)]
pub struct Reader {
    file: File,
    pub path: PathBuf,
    pub header: Header,
    /// The file's device and inode ([`Pending::commit_replacing`]).
    identity: FileIdentity,
}

/// A file's device and inode number: which file a path named.
pub type FileIdentity = (u64, u64);

fn identity_of(metadata: &fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

impl Reader {
    /// Open the snapshot of `key`: `Ok(None)` when there is none (or one of
    /// another format or binary), `Err` when it is damaged.
    pub fn open(cache: &Path, key: &Key) -> Result<Option<Self>, String> {
        let path = path(cache, key);
        Self::open_path_bounded(path, key, u64::MAX)
    }

    /// Graph-local fixed slots use the original full analysis key in the
    /// header. Reject an oversized entry before reading any section.
    pub(crate) fn open_path_bounded(
        path: PathBuf,
        key: &Key,
        max_bytes: u64,
    ) -> Result<Option<Self>, String> {
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("cannot open {}: {error}", path.display())),
        };
        let metadata = file
            .metadata()
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        let len = metadata.len();
        if !metadata.is_file() || len > max_bytes {
            return Err("snapshot entry exceeds its size limit or is not a file".into());
        }
        let identity = identity_of(&metadata);
        let mut bytes = [0u8; HEADER_LEN];
        file.read_exact_at(&mut bytes, 0)
            .map_err(|_| "truncated header".to_owned())?;
        if &bytes[..8] == format::MAGIC
            && u32::from_le_bytes(bytes[8..12].try_into().expect("4"))
                != crate::snapshot::key::FORMAT
        {
            // Another format: absent (the next save replaces it).
            return Ok(None);
        }
        let header = Header::decode(&bytes, len)?;
        if header.binary != key.binary {
            return Ok(None);
        }
        if header.key != key.g {
            return Err("the file holds another key".to_owned());
        }
        Ok(Some(Self {
            file,
            path,
            header,
            identity,
        }))
    }

    /// Which file the reader opened ([`Pending::commit_replacing`]).
    pub fn identity(&self) -> FileIdentity {
        self.identity
    }

    pub(crate) fn modified(&self) -> SystemTime {
        self.file
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    }

    /// Bounded callers check the length before the stored-section allocation.
    pub(crate) fn stored_section_bounded(
        &self,
        kind: Section,
        max_bytes: u64,
    ) -> Result<Vec<u8>, String> {
        if self
            .header
            .slot(kind)
            .is_some_and(|slot| slot.len > max_bytes)
        {
            return Err(format!("{} section exceeds its size limit", kind.name()));
        }
        self.stored_section(kind)
    }

    /// One section's content, checked against its checksum: the PLAN
    /// section inflated (P3, format::decompress_plan).
    pub fn section(&self, kind: Section) -> Result<Vec<u8>, String> {
        let stored = self.stored_section(kind)?;
        if matches!(kind, Section::Plan | Section::Index) {
            format::decompress_plan(stored)
        } else {
            Ok(stored)
        }
    }

    /// One section's bytes as stored, checked against its checksum (a
    /// refresh copies them).
    pub fn stored_section(&self, kind: Section) -> Result<Vec<u8>, String> {
        let slot = self
            .header
            .slot(kind)
            .ok_or_else(|| format!("no {} section", kind.name()))?;
        let len =
            usize::try_from(slot.len).map_err(|_| format!("{} section too large", kind.name()))?;
        let mut bytes = vec![0u8; len];
        self.file
            .read_exact_at(&mut bytes, slot.offset)
            .map_err(|error| format!("cannot read the {} section: {error}", kind.name()))?;
        if format::checksum(&bytes) != slot.checksum {
            return Err(format!("damaged {} section (checksum)", kind.name()));
        }
        Ok(bytes)
    }

    /// Mark the snapshot used (LRU garbage collection keeps it).
    pub fn touch(&self) {
        let _ = self.file.set_modified(SystemTime::now());
    }
}

static TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// A snapshot written to a temporary file in the background, renamed into
/// place by [`Pending::commit`].
#[derive(Debug)]
pub struct Pending {
    cache: PathBuf,
    key: Key,
    path: PathBuf,
    temporary: PathBuf,
    writer: Option<JoinHandle<Result<(u64, String), String>>>,
    /// Where the garbage collection after the commit logs its evictions
    /// ([`Pending::logged`]; none by default).
    log: Option<crate::snapshot::telemetry::Context>,
}

/// Start writing the snapshot of `key` with `sections` (in file order) on a
/// background thread.
pub fn write_in_background(cache: &Path, key: &Key, sections: Vec<(Section, Vec<u8>)>) -> Pending {
    write_with(cache, key, move || Ok((sections, String::new())))
}

/// [`write_in_background`] of the sections `produce` returns on the
/// background thread (with a note for diagnostics, [`Pending::commit`]);
/// an error writes nothing.
pub fn write_with(
    cache: &Path,
    key: &Key,
    produce: impl FnOnce() -> Result<(Vec<(Section, Vec<u8>)>, String), String> + Send + 'static,
) -> Pending {
    let path = path(cache, key);
    let directory = directory(cache);
    let temporary = directory.join(format!(
        "{TEMPORARY_PREFIX}{}-{}-{}",
        key.short(),
        std::process::id(),
        TEMPORARY.fetch_add(1, Ordering::Relaxed)
    ));
    let key = *key;
    let target = temporary.clone();
    let writer = std::thread::Builder::new()
        .name("snapshot-writer".to_owned())
        .spawn(move || {
            let (sections, note) = produce()?;
            let total: u64 = sections.iter().map(|(_, bytes)| bytes.len() as u64).sum();
            if total > MAX_BYTES {
                return Err(format!("{total} bytes, above the {MAX_BYTES} bytes cap"));
            }
            write_file(&directory, &target, &key, &sections).map(|written| (written, note))
        })
        .ok();
    Pending {
        cache: cache.to_path_buf(),
        key,
        path,
        temporary,
        writer,
        log: None,
    }
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX)
        })
}

fn write_file(
    directory: &Path,
    temporary: &Path,
    key: &Key,
    sections: &[(Section, Vec<u8>)],
) -> Result<u64, String> {
    let error = |what: &str, error: io::Error| format!("{what} {}: {error}", temporary.display());
    fs::create_dir_all(directory).map_err(|e| error("cannot create the directory of", e))?;
    let borrowed: Vec<(Section, &[u8])> = sections
        .iter()
        .map(|(kind, bytes)| (*kind, bytes.as_slice()))
        .collect();
    let header = Header::new(key, now_ns(), &borrowed);
    let mut file = File::create(temporary).map_err(|e| error("cannot create", e))?;
    file.write_all(&header.encode())
        .map_err(|e| error("cannot write", e))?;
    let mut total = HEADER_LEN as u64;
    for (_, bytes) in sections {
        file.write_all(bytes)
            .map_err(|e| error("cannot write", e))?;
        total += bytes.len() as u64;
    }
    Ok(total)
}

impl Pending {
    /// Log the evictions of the garbage collection after the commit to
    /// `log`'s event log (crate::snapshot::telemetry, L2-W5h).
    pub fn logged(mut self, log: crate::snapshot::telemetry::Context) -> Self {
        self.log = Some(log);
        self
    }

    /// The command succeeded: wait for the writer, rename the file into
    /// place and collect garbage. Returns the snapshot's size and the
    /// writer's note.
    pub fn commit(mut self) -> Result<(u64, String), String> {
        let written = self.join()?;
        self.rename_and_collect(written)
    }

    /// [`Pending::commit`] of a hit's refresh (crate::snapshot::hit): only
    /// while the key's path still names the file `read` — the hit's reader —
    /// opened (L2-W5h: its device and inode, checked after the writer
    /// finished, right before the rename). A snapshot removed before the
    /// check — healed by a failed hit, set aside by a verify mismatch,
    /// evicted — or replaced by another save is not brought back by the
    /// refresh of the old one. The reader is borrowed until the rename
    /// (L2-W5i): the file it holds open keeps its inode, so no newer file can
    /// reuse the number and pass the check. The check and the rename are two
    /// steps: a removal or a save in the instant between them is overwritten
    /// (docs/ANALYSIS_SNAPSHOT.md, "What a hit does" step 6: at worst one
    /// lost newer snapshot or one more failing hit and heal; the refreshed
    /// file is validated like any other, never a false hit).
    pub fn commit_replacing(mut self, read: &Reader) -> Result<(u64, String), String> {
        let written = self.join()?;
        let current = fs::symlink_metadata(&self.path)
            .ok()
            .map(|metadata| identity_of(&metadata));
        if current != Some(read.identity()) {
            return Err(format!(
                "{} was removed or replaced since the hit read it: not refreshed",
                self.path.display()
            ));
        }
        self.rename_and_collect(written)
    }

    fn rename_and_collect(&mut self, written: (u64, String)) -> Result<(u64, String), String> {
        fs::rename(&self.temporary, &self.path).map_err(|error| {
            let _ = fs::remove_file(&self.temporary);
            format!("cannot rename {}: {error}", self.temporary.display())
        })?;
        let evicted = collect_garbage(&self.cache);
        if let Some(log) = &self.log {
            for eviction in &evicted {
                log.log_evicted(
                    &eviction.key,
                    eviction.reason,
                    &eviction.path,
                    &format!(
                        "by:{},kept:{},bytes:{}",
                        self.key.short(),
                        eviction.kept,
                        eviction.kept_bytes
                    ),
                );
            }
        }
        Ok(written)
    }

    fn join(&mut self) -> Result<(u64, String), String> {
        match self.writer.take() {
            Some(writer) => writer
                .join()
                .unwrap_or_else(|_| Err("the snapshot writer panicked".to_owned())),
            None => Err("cannot start the snapshot writer".to_owned()),
        }
    }
}

impl Drop for Pending {
    /// Not committed (the command failed): discard the temporary file.
    fn drop(&mut self) {
        if self.writer.is_some() {
            let _ = self.join();
        }
        let _ = fs::remove_file(&self.temporary);
    }
}

/// Remove the snapshot of `key` (a stale hit, crate::snapshot::hit).
pub fn remove(cache: &Path, key: &Key) -> io::Result<()> {
    fs::remove_file(path(cache, key))
}

/// What [`set_aside`] did with a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetAside {
    /// Moved to this path in [`mismatched_directory`].
    Moved(PathBuf),
    /// The move failed (`error`), so the snapshot was removed instead.
    Removed { error: String },
}

/// A verify mismatch contradicted the snapshot of `key` (L2-W5i): move it
/// out of the store to
/// `<state>/snapshots/mismatched/<UTC time>-<key>-<pid>.snap` — the next
/// lookup misses, as when it was removed, but the file stays for the soak's
/// report beside `verify-details.log` (crate::snapshot::telemetry) — and
/// keep the [`MISMATCHED_KEEP`] newest there. A move that fails removes the
/// snapshot instead; `Err` when it is neither (gone already: another
/// command removed it).
pub fn set_aside(cache: &Path, key: &Key) -> io::Result<SetAside> {
    let source = path(cache, key);
    let directory = mismatched_directory(cache);
    let moved = fs::create_dir_all(&directory).and_then(|()| {
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        // `2026-10-24T180901.123456789Z-<key>-<pid>.snap`: the names sort
        // by time.
        let target = directory.join(format!(
            "{}.{:09}Z-{}-{}{SUFFIX}",
            crate::snapshot::telemetry::utc_timestamp(now.as_secs() as i64)
                .trim_end_matches('Z')
                .replace(':', ""),
            now.subsec_nanos(),
            key.short(),
            std::process::id()
        ));
        fs::rename(&source, &target).map(|()| target)
    });
    match moved {
        Ok(target) => {
            prune_mismatched(&directory, MISMATCHED_KEEP);
            Ok(SetAside::Moved(target))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound && !source.exists() => Err(error),
        Err(error) => fs::remove_file(&source).map(|()| SetAside::Removed {
            error: error.to_string(),
        }),
    }
}

/// Keep the `keep` newest snapshots (by name: [`set_aside`]'s names sort by
/// time) in `directory`.
fn prune_mismatched(directory: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut names: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(SUFFIX))
        })
        .collect();
    names.sort();
    let excess = names.len().saturating_sub(keep);
    for path in &names[..excess] {
        let _ = fs::remove_file(path);
    }
}

/// A snapshot [`collect_garbage`] removed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evicted {
    /// Its key (the file name: the short form of G).
    pub key: String,
    pub path: PathBuf,
    /// `count` (beyond [`KEEP`]) or `bytes` (beyond the [`budget`]).
    pub reason: &'static str,
    /// The snapshots and bytes kept.
    pub kept: usize,
    pub kept_bytes: u64,
}

/// The byte budget of a store holding `stored` bytes in `directory`
/// (P3): [`BUDGET_PERCENT`] of what its file system has available to this
/// user plus `stored` (the store may use what it already holds), between
/// [`BUDGET_FLOOR`] and [`BUDGET_CAP`] — [`BUDGET_CAP`] when the file
/// system cannot be asked. A nearly full disk keeps few snapshots; a large
/// one keeps up to 4 GiB of them (1 GiB before P3, whatever the disk).
pub fn budget(directory: &Path, stored: u64) -> u64 {
    match rustix::fs::statvfs(directory) {
        Ok(stat) => {
            let available = stat.f_bavail.saturating_mul(stat.f_frsize);
            (available.saturating_add(stored) / 100 * BUDGET_PERCENT)
                .clamp(BUDGET_FLOOR, BUDGET_CAP)
        }
        Err(_) => BUDGET_CAP,
    }
}

/// Keep at most [`KEEP`] snapshots and the store's byte [`budget`], most
/// recently used first; remove temporary files older than an hour. Returns
/// the snapshots it removed (the event log names them, L2-W5h: an `absent`
/// miss of an evicted key is no snapshot that was never saved).
pub fn collect_garbage(cache: &Path) -> Vec<Evicted> {
    collect_garbage_within(cache, KEEP, None)
}

/// [`collect_garbage`] with `keep` snapshots and `bytes` (default: the
/// [`budget`]).
fn collect_garbage_within(cache: &Path, keep: usize, bytes: Option<u64>) -> Vec<Evicted> {
    let directory = directory(cache);
    let Ok(entries) = fs::read_dir(&directory) else {
        return Vec::new();
    };
    let now = SystemTime::now();
    let mut snapshots: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if name.starts_with(TEMPORARY_PREFIX) {
            if now
                .duration_since(modified)
                .is_ok_and(|age| age > STALE_TEMPORARY)
            {
                let _ = fs::remove_file(entry.path());
            }
        } else if name.ends_with(SUFFIX) && metadata.is_file() {
            snapshots.push((modified, metadata.len(), entry.path()));
        }
    }
    // Newest first; ties by name for a stable order.
    snapshots.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.2.cmp(&b.2)));
    let limit = bytes.unwrap_or_else(|| {
        budget(
            &directory,
            snapshots.iter().map(|(_, len, _)| *len).sum::<u64>(),
        )
    });
    let mut kept = 0usize;
    let mut bytes = 0u64;
    let mut removed = Vec::new();
    for (_, len, path) in snapshots {
        if kept < keep && bytes.saturating_add(len) <= limit {
            kept += 1;
            bytes += len;
        } else {
            let reason = if kept >= keep { "count" } else { "bytes" };
            // Only what this collection removed (another may race it).
            if fs::remove_file(&path).is_ok() {
                let key = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.strip_suffix(SUFFIX))
                    .unwrap_or_default()
                    .to_owned();
                removed.push((key, path, reason));
            }
        }
    }
    removed
        .into_iter()
        .map(|(key, path, reason)| Evicted {
            key,
            path,
            reason,
            kept,
            kept_bytes: bytes,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> Key {
        Key {
            g: [seed; 32],
            binary: [7; 32],
        }
    }

    fn sections(plan: &[u8]) -> Vec<(Section, Vec<u8>)> {
        vec![
            (Section::Inputs, b"in".to_vec()),
            (Section::Events, Vec::new()),
            (Section::Plan, plan.to_vec()),
        ]
    }

    #[test]
    fn written_snapshots_read_back_and_damage_is_a_miss() {
        let cache = tempfile::tempdir().unwrap();
        let cache = cache.path();
        assert!(Reader::open(cache, &key(1)).unwrap().is_none());
        let (size, _) = write_in_background(cache, &key(1), sections(b"plan"))
            .commit()
            .unwrap();
        assert_eq!(size, (HEADER_LEN + 6) as u64);
        let reader = Reader::open(cache, &key(1)).unwrap().unwrap();
        assert_eq!(reader.section(Section::Plan).unwrap(), b"plan");
        assert_eq!(reader.section(Section::Inputs).unwrap(), b"in");
        assert!(reader.section(Section::Stats).is_err());
        // Another binary: absent.
        let other = Key {
            binary: [8; 32],
            ..key(1)
        };
        assert!(Reader::open(cache, &other).unwrap().is_none());
        // A damaged section is refused; a truncated file too.
        let file = path(cache, &key(1));
        let mut bytes = fs::read(&file).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        fs::write(&file, &bytes).unwrap();
        let reader = Reader::open(cache, &key(1)).unwrap().unwrap();
        assert!(
            reader
                .section(Section::Plan)
                .unwrap_err()
                .contains("checksum")
        );
        fs::write(&file, &bytes[..bytes.len() - 2]).unwrap();
        assert!(Reader::open(cache, &key(1)).is_err());
        fs::write(&file, b"short").unwrap();
        assert!(Reader::open(cache, &key(1)).is_err());
        // Nothing temporary is left behind.
        let leftovers: Vec<_> = fs::read_dir(directory(cache))
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty());
    }

    /// A hit reads its sections by offset from the file it opened: garbage
    /// collection (or a concurrent save's rename) removing the path meanwhile
    /// never breaks it (L2-W5f).
    #[test]
    fn an_open_reader_survives_its_collection() {
        let cache = tempfile::tempdir().unwrap();
        let cache = cache.path();
        write_in_background(cache, &key(1), sections(b"plan"))
            .commit()
            .unwrap();
        let reader = Reader::open(cache, &key(1)).unwrap().unwrap();
        fs::remove_file(path(cache, &key(1))).unwrap();
        // A newer snapshot of the key renamed into place meanwhile.
        write_in_background(cache, &key(1), sections(b"newer"))
            .commit()
            .unwrap();
        assert_eq!(reader.section(Section::Plan).unwrap(), b"plan");
        assert_eq!(reader.section(Section::Inputs).unwrap(), b"in");
        reader.touch();
        let again = Reader::open(cache, &key(1)).unwrap().unwrap();
        assert_eq!(again.section(Section::Plan).unwrap(), b"newer");
    }

    /// L2-W5h: a hit's refresh replaces only the file the hit read; a
    /// snapshot removed (healed, evicted) or replaced (saved anew) before
    /// the check stays removed or replaced, and no temporary file is left
    /// behind. (L2-W5i: the check borrows the hit's reader, whose open file
    /// keeps its inode number from being reused by a newer file.)
    #[test]
    fn a_refresh_never_brings_back_a_removed_or_replaced_snapshot() {
        let cache = tempfile::tempdir().unwrap();
        let cache = cache.path();
        let refresh = |reader: &Reader, plan: &[u8]| {
            write_in_background(cache, &key(1), sections(plan)).commit_replacing(reader)
        };
        write_in_background(cache, &key(1), sections(b"old"))
            .commit()
            .unwrap();
        // Unchanged since it was read: replaced by the refresh.
        let reader = Reader::open(cache, &key(1)).unwrap().unwrap();
        refresh(&reader, b"refreshed").unwrap();
        let reader = Reader::open(cache, &key(1)).unwrap().unwrap();
        assert_eq!(reader.section(Section::Plan).unwrap(), b"refreshed");
        // Removed meanwhile (a heal): stays removed.
        fs::remove_file(path(cache, &key(1))).unwrap();
        assert!(refresh(&reader, b"resurrected").is_err());
        assert!(!path(cache, &key(1)).exists());
        // Replaced meanwhile (another save): the other save stays.
        write_in_background(cache, &key(1), sections(b"newer"))
            .commit()
            .unwrap();
        assert!(refresh(&reader, b"stale").is_err());
        let again = Reader::open(cache, &key(1)).unwrap().unwrap();
        assert_eq!(again.section(Section::Plan).unwrap(), b"newer");
        let leftovers = fs::read_dir(directory(cache))
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".tmp-"))
            .count();
        assert_eq!(leftovers, 0);
    }

    /// L2-W5i: a snapshot a verify mismatch contradicted is moved out of the
    /// store, not deleted: the lookup misses (absent), the garbage
    /// collection never counts or collects it, and only the
    /// [`MISMATCHED_KEEP`] newest stay; one gone already is an error.
    #[test]
    fn mismatching_snapshots_are_set_aside_and_the_newest_kept() {
        let cache = tempfile::tempdir().unwrap();
        let cache = cache.path();
        let mut moved = Vec::new();
        for seed in 1..=(MISMATCHED_KEEP as u8 + 2) {
            write_in_background(cache, &key(seed), sections(&[seed]))
                .commit()
                .unwrap();
            let SetAside::Moved(target) = set_aside(cache, &key(seed)).unwrap() else {
                panic!("not moved");
            };
            assert!(Reader::open(cache, &key(seed)).unwrap().is_none());
            assert!(
                target.starts_with(mismatched_directory(cache))
                    && target
                        .file_name()
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .contains(&key(seed).short()),
                "{target:?}"
            );
            // The moved file is the snapshot, byte for byte.
            assert!(fs::read(&target).unwrap().ends_with(&[seed]));
            moved.push(target);
        }
        let kept: Vec<bool> = moved.iter().map(|path| path.exists()).collect();
        assert_eq!(kept, [false, false, true, true, true]);
        // Outside the store: the garbage collection neither counts them nor
        // removes them.
        for seed in 0..(KEEP as u8) {
            write_in_background(cache, &key(100 + seed), sections(b"p"))
                .commit()
                .unwrap();
        }
        assert!(collect_garbage(cache).is_empty());
        assert_eq!(
            fs::read_dir(mismatched_directory(cache)).unwrap().count(),
            MISMATCHED_KEEP
        );
        // Gone already (another command removed it): an error, nothing moved.
        assert!(set_aside(cache, &key(1)).is_err());
        assert_eq!(
            fs::read_dir(mismatched_directory(cache)).unwrap().count(),
            MISMATCHED_KEEP
        );
    }

    #[test]
    fn an_uncommitted_write_leaves_nothing() {
        let cache = tempfile::tempdir().unwrap();
        drop(write_in_background(cache.path(), &key(2), sections(b"x")));
        assert!(Reader::open(cache.path(), &key(2)).unwrap().is_none());
        assert_eq!(fs::read_dir(directory(cache.path())).unwrap().count(), 0);
    }

    #[test]
    fn garbage_collection_keeps_the_most_recently_used() {
        let cache = tempfile::tempdir().unwrap();
        let cache = cache.path();
        for seed in 0..(KEEP as u8 + 3) {
            write_in_background(cache, &key(seed), sections(b"p"))
                .commit()
                .unwrap();
            let file = File::options()
                .write(true)
                .open(path(cache, &key(seed)))
                .unwrap();
            // Older seeds were used longer ago; seed 0 was just used.
            let age = if seed == 0 { 0 } else { 1000 - u64::from(seed) };
            file.set_modified(SystemTime::now() - Duration::from_secs(age))
                .unwrap();
        }
        // (Each commit collected: seeds 1 to 3 went then.)
        assert!(collect_garbage(cache).is_empty());
        let kept: Vec<bool> = (0..(KEEP as u8 + 3))
            .map(|seed| path(cache, &key(seed)).exists())
            .collect();
        assert_eq!(kept.iter().filter(|kept| **kept).count(), KEEP);
        assert!(kept[0], "the most recently used survives");
        assert!(!kept[1] && !kept[2] && !kept[3], "{kept:?}");
        // L2-W5h: what a collection removed, by key (the file name) and
        // limit: two newer files evict the two least recently used.
        for n in 0..2 {
            fs::write(directory(cache).join(format!("standin{n}{SUFFIX}")), b"x").unwrap();
        }
        let evicted = collect_garbage(cache);
        let mut names: Vec<(String, &str, usize)> = evicted
            .iter()
            .map(|evicted| (evicted.key.clone(), evicted.reason, evicted.kept))
            .collect();
        names.sort();
        assert_eq!(
            names,
            [4, 5]
                .map(|seed| (key(seed).short(), "count", KEEP))
                .to_vec()
        );
        assert!(evicted.iter().all(|evicted| !evicted.path.exists()));
        // A commit with an event log names what its collection evicted and
        // the key that collected it.
        let log = crate::snapshot::telemetry::Context {
            cache: cache.to_path_buf(),
            mode: crate::snapshot::telemetry::MODE_ON,
            command: "build".to_owned(),
            targets: vec!["//:x".to_owned()],
        };
        write_in_background(cache, &key(250), sections(b"p"))
            .logged(log)
            .commit()
            .unwrap();
        let events = fs::read_to_string(crate::snapshot::telemetry::log_path(
            cache,
            crate::snapshot::telemetry::EVENTS_LOG,
        ))
        .unwrap();
        assert_eq!(events.lines().count(), 1, "{events}");
        assert!(
            events.contains(&format!(
                " kind=evicted mode=on key={} command=- reason=count path=",
                key(6).short()
            )) && events.contains(&format!(" detail=by:{},kept:{KEEP},", key(250).short())),
            "{events}"
        );
        // A stale temporary file goes, a fresh one stays.
        let stale = directory(cache).join(".tmp-stale");
        let fresh = directory(cache).join(".tmp-fresh");
        fs::write(&stale, b"").unwrap();
        fs::write(&fresh, b"").unwrap();
        File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(7200))
            .unwrap();
        collect_garbage(cache);
        assert!(!stale.exists() && fresh.exists());
    }

    /// P3: the byte budget is a share of the disk, capped and floored; a
    /// collection over it evicts the least recently used by `bytes`.
    #[test]
    fn the_byte_budget_follows_the_disk_and_evicts_by_bytes() {
        let cache = tempfile::tempdir().unwrap();
        let cache = cache.path();
        fs::create_dir_all(directory(cache)).unwrap();
        let budget = budget(&directory(cache), 0);
        assert!((BUDGET_FLOOR..=BUDGET_CAP).contains(&budget), "{budget}");
        // The store's own bytes count as available.
        assert!(super::budget(&directory(cache), 1 << 40) == BUDGET_CAP);
        assert_eq!(
            super::budget(Path::new("/nonexistent/snapshots"), 0),
            BUDGET_CAP
        );
        for seed in 0..4u8 {
            fs::write(path(cache, &key(seed)), vec![0u8; 100]).unwrap();
            File::options()
                .write(true)
                .open(path(cache, &key(seed)))
                .unwrap()
                .set_modified(SystemTime::now() - Duration::from_secs(100 - u64::from(seed)))
                .unwrap();
        }
        let evicted = collect_garbage_within(cache, KEEP, Some(250));
        let mut names: Vec<(String, &str, usize, u64)> = evicted
            .iter()
            .map(|evicted| {
                (
                    evicted.key.clone(),
                    evicted.reason,
                    evicted.kept,
                    evicted.kept_bytes,
                )
            })
            .collect();
        names.sort();
        assert_eq!(
            names,
            [0, 1]
                .map(|seed| (key(seed).short(), "bytes", 2, 200))
                .to_vec()
        );
    }
}
