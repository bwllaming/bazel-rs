//! The snapshot store (PLAN-L2 §1d "Rules for the store"): one file per key
//! G, `<cache>/snapshots/v1/<first 16 bytes of G in hex>.snap`, in the
//! binary format of crate::snapshot::format.
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
//!   [`BUDGET`] bytes per cache, least recently used first (a hit sets its
//!   file's mtime), and temporary files older than an hour.

use crate::snapshot::format::{self, HEADER_LEN, Header, Section};
use crate::snapshot::key::Key;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

/// Snapshots kept per cache.
pub const KEEP: usize = 16;
/// Bytes of snapshots kept per cache.
pub const BUDGET: u64 = 1 << 30;
/// The largest snapshot written (PLAN-L2 §1b "Size").
pub const MAX_BYTES: u64 = 512 << 20;
const TEMPORARY_PREFIX: &str = ".tmp-";
const STALE_TEMPORARY: Duration = Duration::from_secs(3600);
const SUFFIX: &str = ".snap";

/// `<cache>/snapshots`: the binary identity cache and the versioned store.
pub fn root(cache: &Path) -> PathBuf {
    cache.join("snapshots")
}

/// The directory of this format's snapshot files.
pub fn directory(cache: &Path) -> PathBuf {
    root(cache).join("v1")
}

/// The file of `key`.
pub fn path(cache: &Path, key: &Key) -> PathBuf {
    directory(cache).join(format!("{}{SUFFIX}", key.short()))
}

/// An open snapshot whose header matched the key.
#[derive(Debug)]
pub struct Reader {
    file: File,
    pub path: PathBuf,
    pub header: Header,
}

impl Reader {
    /// Open the snapshot of `key`: `Ok(None)` when there is none (or one of
    /// another format or binary), `Err` when it is damaged.
    pub fn open(cache: &Path, key: &Key) -> Result<Option<Self>, String> {
        let path = path(cache, key);
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("cannot open {}: {error}", path.display())),
        };
        let len = file
            .metadata()
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?
            .len();
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
        Ok(Some(Self { file, path, header }))
    }

    /// One section's bytes, checked against its checksum.
    pub fn section(&self, kind: Section) -> Result<Vec<u8>, String> {
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
    path: PathBuf,
    temporary: PathBuf,
    writer: Option<JoinHandle<Result<(u64, String), String>>>,
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
        path,
        temporary,
        writer,
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
    /// The command succeeded: wait for the writer, rename the file into
    /// place and collect garbage. Returns the snapshot's size and the
    /// writer's note.
    pub fn commit(mut self) -> Result<(u64, String), String> {
        let written = self.join()?;
        fs::rename(&self.temporary, &self.path).map_err(|error| {
            let _ = fs::remove_file(&self.temporary);
            format!("cannot rename {}: {error}", self.temporary.display())
        })?;
        collect_garbage(&self.cache);
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

/// Keep at most [`KEEP`] snapshots and [`BUDGET`] bytes, most recently used
/// first; remove temporary files older than an hour.
pub fn collect_garbage(cache: &Path) {
    let directory = directory(cache);
    let Ok(entries) = fs::read_dir(&directory) else {
        return;
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
    let mut kept = 0usize;
    let mut bytes = 0u64;
    for (_, len, path) in snapshots {
        if kept < KEEP && bytes.saturating_add(len) <= BUDGET {
            kept += 1;
            bytes += len;
        } else {
            let _ = fs::remove_file(path);
        }
    }
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
        collect_garbage(cache);
        let kept: Vec<bool> = (0..(KEEP as u8 + 3))
            .map(|seed| path(cache, &key(seed)).exists())
            .collect();
        assert_eq!(kept.iter().filter(|kept| **kept).count(), KEEP);
        assert!(kept[0], "the most recently used survives");
        assert!(!kept[1] && !kept[2] && !kept[3], "{kept:?}");
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
}
