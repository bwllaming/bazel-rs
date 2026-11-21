//! Process-global string interning (docs/MEMORY.md, "String interning").
//!
//! An [`IStr`] is one pointer to an interned copy of its text: equal texts
//! interned anywhere in the process are one copy, and an `IStr` costs 8
//! bytes where a `String` costs 24 plus its own heap allocation. The copies
//! live in leaked arena chunks for the rest of the process (every command is
//! its own process), so intern only long-lived strings that repeat — a
//! planned action's arguments (OpenROAD: 5.3 M argument strings, 92 K
//! distinct) — never transient ones.
//!
//! An `IStr` hashes, compares, orders, prints (`Display`, `Debug`) and
//! serializes exactly as the `String` of its text, so it replaces a `String`
//! field without changing a map's order, a key or an encoded byte.
//!
//! The table is sharded ([`SHARDS`] mutexes, picked by the text's hash): an
//! intern takes one shard's lock for a lookup and, for a new text, a copy
//! into that shard's arena chunk.
#![allow(unsafe_code)]

use std::borrow::Borrow;
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

/// Shards of the table (a power of two).
const SHARDS: usize = 64;
/// Words (8 bytes) of an arena chunk: 256 KiB.
const CHUNK_WORDS: usize = 1 << 15;
/// Texts above this many words get an allocation of their own.
const LARGE_WORDS: usize = CHUNK_WORDS / 8;

/// An interned string: a pointer to its length word, followed by its UTF-8
/// bytes, in a leaked arena.
#[derive(Clone, Copy)]
pub struct IStr(NonNull<u64>);

// SAFETY: an IStr points to immutable, never-freed memory.
unsafe impl Send for IStr {}
// SAFETY: as above.
unsafe impl Sync for IStr {}

/// The empty string's storage (length 0, no bytes).
static EMPTY: u64 = 0;

struct Shard {
    table: hashbrown::HashTable<IStr>,
    /// The unused rest of the shard's current chunk.
    rest: &'static mut [u64],
}

static TABLE: LazyLock<[Mutex<Shard>; SHARDS]> = LazyLock::new(|| {
    std::array::from_fn(|_| {
        Mutex::new(Shard {
            table: hashbrown::HashTable::new(),
            rest: &mut [],
        })
    })
});

/// Distinct texts interned and their bytes (arena words included).
static INTERNED: AtomicU64 = AtomicU64::new(0);
static INTERNED_BYTES: AtomicU64 = AtomicU64::new(0);

/// The text's hash: FxHash over the bytes, then a 64-bit finalizer so the
/// shard bits and the table's bits are both well mixed.
fn hash_text(text: &str) -> u64 {
    let mut hasher = crate::fast_hash::FxHasher::default();
    hasher.write(text.as_bytes());
    hasher.write_usize(text.len());
    let mut hash = hasher.finish();
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    hash ^ (hash >> 33)
}

/// The shard of a hash: bits the table itself does not use for buckets
/// (low bits) or control bytes (the top 7).
fn shard_of(hash: u64) -> usize {
    ((hash >> 40) as usize) & (SHARDS - 1)
}

impl Shard {
    /// Copy `text` into this shard's arena.
    fn store(&mut self, text: &str) -> IStr {
        let words = 1 + text.len().div_ceil(8);
        let slot: &'static mut [u64] = if words > LARGE_WORDS {
            Box::leak(vec![0u64; words].into_boxed_slice())
        } else {
            if self.rest.len() < words {
                self.rest = Box::leak(vec![0u64; CHUNK_WORDS].into_boxed_slice());
            }
            let rest = std::mem::take(&mut self.rest);
            let (slot, rest) = rest.split_at_mut(words);
            self.rest = rest;
            slot
        };
        slot[0] = text.len() as u64;
        // SAFETY: `slot` has `words - 1 >= text.len() / 8` words after the
        // length word, exclusively ours, and u8 has no alignment needs.
        unsafe {
            std::ptr::copy_nonoverlapping(
                text.as_ptr(),
                slot.as_mut_ptr().add(1).cast::<u8>(),
                text.len(),
            );
        }
        INTERNED.fetch_add(1, Ordering::Relaxed);
        INTERNED_BYTES.fetch_add(words as u64 * 8, Ordering::Relaxed);
        IStr(NonNull::from(&mut slot[0]))
    }
}

impl IStr {
    /// The interned copy of `text`.
    pub fn new(text: &str) -> IStr {
        if text.is_empty() {
            return IStr::default();
        }
        let hash = hash_text(text);
        let mut shard = TABLE[shard_of(hash)]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(found) = shard.table.find(hash, |entry| entry.as_str() == text) {
            return *found;
        }
        let interned = shard.store(text);
        shard
            .table
            .insert_unique(hash, interned, |entry| hash_text(entry.as_str()));
        interned
    }

    /// The text (it lives for the rest of the process).
    pub fn as_str(&self) -> &'static str {
        let header = self.0.as_ptr();
        // SAFETY: an IStr points to a length word followed by that many
        // bytes of valid UTF-8 (copied from a `&str` by `Shard::store`, or
        // EMPTY), in memory never freed or written again.
        unsafe {
            let len = *header as usize;
            let bytes = std::slice::from_raw_parts(header.add(1).cast::<u8>(), len);
            std::str::from_utf8_unchecked(bytes)
        }
    }

    /// Intern every string of `strings`, into a vector of exactly their
    /// number (never the strings' own buffer: collecting a
    /// `Vec<String>`'s `into_iter()` reuses its allocation, 24 bytes a slot).
    pub fn all(strings: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<IStr> {
        let strings = strings.into_iter();
        let mut interned = Vec::with_capacity(strings.size_hint().0);
        for text in strings {
            interned.push(IStr::new(text.as_ref()));
        }
        interned.shrink_to_fit();
        interned
    }
}

/// The distinct texts interned so far and their arena bytes.
pub fn stats() -> (u64, u64) {
    (
        INTERNED.load(Ordering::Relaxed),
        INTERNED_BYTES.load(Ordering::Relaxed),
    )
}

impl Default for IStr {
    fn default() -> Self {
        IStr(NonNull::from(&EMPTY))
    }
}

impl std::ops::Deref for IStr {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for IStr {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<OsStr> for IStr {
    fn as_ref(&self) -> &OsStr {
        OsStr::new(self.as_str())
    }
}

impl AsRef<Path> for IStr {
    fn as_ref(&self) -> &Path {
        Path::new(self.as_str())
    }
}

impl AsRef<[u8]> for IStr {
    fn as_ref(&self) -> &[u8] {
        self.as_str().as_bytes()
    }
}

impl Borrow<str> for IStr {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl From<&str> for IStr {
    fn from(text: &str) -> Self {
        IStr::new(text)
    }
}

impl From<&String> for IStr {
    fn from(text: &String) -> Self {
        IStr::new(text)
    }
}

impl From<String> for IStr {
    fn from(text: String) -> Self {
        IStr::new(&text)
    }
}

impl From<IStr> for String {
    fn from(text: IStr) -> Self {
        text.as_str().to_owned()
    }
}

// Equal texts are one interned copy (the empty text is always EMPTY), so
// equality is identity.
impl PartialEq for IStr {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for IStr {}

impl PartialEq<str> for IStr {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for IStr {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for IStr {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<IStr> for str {
    fn eq(&self, other: &IStr) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<IStr> for &str {
    fn eq(&self, other: &IStr) -> bool {
        *self == other.as_str()
    }
}

impl PartialEq<IStr> for String {
    fn eq(&self, other: &IStr) -> bool {
        self == other.as_str()
    }
}

impl PartialOrd for IStr {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for IStr {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if self.0 == other.0 {
            return std::cmp::Ordering::Equal;
        }
        self.as_str().cmp(other.as_str())
    }
}

impl Hash for IStr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl std::fmt::Display for IStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self.as_str(), f)
    }
}

impl std::fmt::Debug for IStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl serde::Serialize for IStr {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for IStr {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl serde::de::Visitor<'_> for Visitor {
            type Value = IStr;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a string")
            }

            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<IStr, E> {
                Ok(IStr::new(text))
            }

            fn visit_bytes<E: serde::de::Error>(self, bytes: &[u8]) -> Result<IStr, E> {
                std::str::from_utf8(bytes)
                    .map(IStr::new)
                    .map_err(|_| E::invalid_value(serde::de::Unexpected::Bytes(bytes), &self))
            }
        }

        deserializer.deserialize_string(Visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::IStr;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    fn hash_of<T: Hash + ?Sized>(value: &T) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn equal_texts_are_one_copy() {
        let owned = String::from("-Iexternal/boost");
        let a = IStr::new(&owned);
        let b = IStr::from("-Iexternal/boost");
        assert_eq!(a, b);
        assert!(std::ptr::eq(a.as_str(), b.as_str()));
        assert_ne!(a, IStr::new("-Iexternal/boost2"));
        assert_eq!(IStr::new(""), IStr::default());
        assert_eq!(IStr::default().as_str(), "");
    }

    #[test]
    fn behaves_as_its_string() {
        let texts = ["", "a", "b", "ab", "雪 $HOME", "x".repeat(70_000).leak()];
        for text in texts {
            let interned = IStr::new(text);
            let string = text.to_owned();
            assert_eq!(interned.as_str(), text);
            assert_eq!(hash_of(&interned), hash_of(&string));
            assert_eq!(format!("{interned}"), format!("{string}"));
            assert_eq!(format!("{interned:?}"), format!("{string:?}"));
            assert_eq!(
                postcard::to_stdvec(&interned).unwrap(),
                postcard::to_stdvec(&string).unwrap()
            );
            let decoded: IStr =
                postcard::from_bytes(&postcard::to_stdvec(&string).unwrap()).unwrap();
            assert_eq!(decoded, interned);
            assert_eq!(
                serde_json::to_string(&interned).unwrap(),
                serde_json::to_string(&string).unwrap()
            );
            for other in texts {
                assert_eq!(
                    interned.cmp(&IStr::new(other)),
                    string.as_str().cmp(other),
                    "{text:?} vs {other:?}"
                );
            }
        }
    }

    #[test]
    fn all_allocates_exactly() {
        let strings: Vec<String> = (0..100).map(|index| format!("all-{index}")).collect();
        let interned = IStr::all(strings);
        assert_eq!(interned.len(), 100);
        assert_eq!(interned.capacity(), 100);
        assert_eq!(interned[7], "all-7");
    }

    #[test]
    fn concurrent_interning_agrees() {
        let threads: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    (0..2000)
                        .map(|index| IStr::new(&format!("concurrent-{index}")))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let results: Vec<Vec<IStr>> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        for result in &results[1..] {
            assert_eq!(result, &results[0]);
        }
    }
}
