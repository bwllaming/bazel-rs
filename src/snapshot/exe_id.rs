//! The bazel-rs binary identity (PLAN-L2 §1a): blake3 of `/proc/self/exe`,
//! part of the snapshot key G and of every snapshot file's header. It covers
//! everything compiled in — the embedded `@bazel_tools`, `genrule-setup.sh`,
//! every natively implemented rule — so a new binary never reads an old
//! snapshot.
//!
//! Hashing a 50 MB release binary costs 10–40 ms, so the identity is cached
//! in `<cache>/snapshots/exe.id`, keyed by the binary's stat key (device,
//! inode, size, mtime, ctime): a steady-state lookup is one stat and one
//! 104-byte read. An entry is only written for a binary that was settled
//! when it was hashed — its mtime and ctime at least two seconds older than
//! the start of the read (the racy rule of crate::observed_fs) — so an
//! in-place rewrite within the timestamp granularity can never match it.
//!
//! The file is fixed-width, little-endian (the user's directive: binary
//! formats only):
//!
//! | offset | content |
//! |---|---|
//! | 0 | magic `BZRSEXID` |
//! | 8 | format (u32), reserved (u32) |
//! | 16 | device, inode, size (u64 each), mtime_ns, ctime_ns (i64 each) |
//! | 56 | blake3 of the binary (32 bytes) |
//! | 88 | the first 16 bytes of blake3 of bytes 0..88 |
//!
//! Written to a temporary file in the same directory and renamed into place;
//! a missing, short or damaged file is ignored (the binary is hashed again).

use crate::observed_fs::StatKey;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;

const MAGIC: &[u8; 8] = b"BZRSEXID";
const FORMAT: u32 = 1;
const FILE_NAME: &str = "exe.id";
const BODY: usize = 88;
const LENGTH: usize = BODY + 16;
/// What the identity is read from: the running binary, also when its path
/// was replaced or deleted since it started.
const EXE: &str = "/proc/self/exe";

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX)
        })
}

fn checksum(bytes: &[u8]) -> [u8; 16] {
    let hash = blake3::hash(bytes);
    hash.as_bytes()[..16].try_into().expect("16 bytes")
}

fn encode(key: &StatKey, id: &[u8; 32]) -> [u8; LENGTH] {
    let mut bytes = [0u8; LENGTH];
    bytes[..8].copy_from_slice(MAGIC);
    bytes[8..12].copy_from_slice(&FORMAT.to_le_bytes());
    bytes[16..24].copy_from_slice(&key.dev.to_le_bytes());
    bytes[24..32].copy_from_slice(&key.ino.to_le_bytes());
    bytes[32..40].copy_from_slice(&key.size.to_le_bytes());
    bytes[40..48].copy_from_slice(&key.mtime_ns.to_le_bytes());
    bytes[48..56].copy_from_slice(&key.ctime_ns.to_le_bytes());
    bytes[56..88].copy_from_slice(id);
    let sum = checksum(&bytes[..BODY]);
    bytes[BODY..].copy_from_slice(&sum);
    bytes
}

/// The cached identity, when the entry is intact and names `key`.
fn decode(bytes: &[u8], key: &StatKey) -> Option<[u8; 32]> {
    if bytes.len() != LENGTH || &bytes[..8] != MAGIC || bytes[BODY..] != checksum(&bytes[..BODY]) {
        return None;
    }
    let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
    let format = u32::from_le_bytes(bytes[8..12].try_into().expect("4 bytes"));
    let cached = StatKey {
        dev: u64_at(16),
        ino: u64_at(24),
        size: u64_at(32),
        mtime_ns: u64_at(40) as i64,
        ctime_ns: u64_at(48) as i64,
    };
    (format == FORMAT && cached == *key).then(|| bytes[56..88].try_into().expect("32 bytes"))
}

/// blake3 of the open file, read in large blocks (blake3's SIMD paths
/// want them).
fn hash(file: &mut File) -> io::Result<[u8; 32]> {
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                hasher.update(&buffer[..count]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(*hasher.finalize().as_bytes())
}

/// The identity of the running binary, through the cache in `directory`
/// (`<cache>/snapshots`, created when an entry is written).
pub fn binary_id(directory: &Path) -> io::Result<[u8; 32]> {
    let key = StatKey::of(&fs::metadata(EXE)?);
    let cache = directory.join(FILE_NAME);
    if let Ok(bytes) = fs::read(&cache)
        && let Some(id) = decode(&bytes, &key)
    {
        return Ok(id);
    }
    let start = now_ns();
    let mut file = File::open(EXE)?;
    let id = hash(&mut file)?;
    let after = StatKey::of(&file.metadata()?);
    if after == key && !key.racy(start) {
        // Best effort: a failure only means hashing again next time.
        let _ = write_entry(directory, &cache, &encode(&key, &id));
    }
    Ok(id)
}

fn write_entry(directory: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    fs::create_dir_all(directory)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".tmp-exe.id.")
        .tempfile_in(directory)?;
    temporary.write_all(bytes)?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_round_trip_and_damage_or_another_binary_is_ignored() {
        let key = StatKey {
            dev: 1,
            ino: 2,
            size: 3,
            mtime_ns: 4,
            ctime_ns: -5,
        };
        let id = [7u8; 32];
        let bytes = encode(&key, &id);
        assert_eq!(decode(&bytes, &key), Some(id));
        let other = StatKey { ino: 9, ..key };
        assert_eq!(decode(&bytes, &other), None);
        for at in [0, 9, 20, 60, 90] {
            let mut damaged = bytes;
            damaged[at] ^= 1;
            assert_eq!(decode(&damaged, &key), None, "byte {at}");
        }
        assert_eq!(decode(&bytes[..LENGTH - 1], &key), None);
    }

    #[test]
    fn the_running_binary_is_identified_and_cached_once_settled() {
        let directory = tempfile::tempdir().unwrap();
        // The test binary is older than the racy margin unless it was just
        // linked: then nothing is cached yet.
        let key = StatKey::of(&fs::metadata(EXE).unwrap());
        let settled = !key.racy(now_ns());
        let id = binary_id(directory.path()).unwrap();
        assert_eq!(binary_id(directory.path()).unwrap(), id);
        let expected = *blake3::hash(&fs::read(EXE).unwrap()).as_bytes();
        assert_eq!(id, expected);
        let cached = fs::read(directory.path().join(FILE_NAME)).ok();
        if settled {
            assert_eq!(decode(&cached.unwrap(), &key), Some(id));
        }
    }
}
