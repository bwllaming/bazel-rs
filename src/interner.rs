//! Process-global string interning (docs/MEMORY.md, "String interning").
//!
//! An [`IStr`] is the 4-byte id of an interned copy of its text: equal texts
//! interned anywhere in the process are one copy and one id, and an `IStr`
//! costs 4 bytes where a `String` costs 24 plus its own heap allocation. The copies
//! live in leaked arena chunks for the rest of the process (every command is
//! its own process), so intern only long-lived strings that repeat — a
//! planned action's arguments (OpenROAD: 5.3 M argument strings, 92 K
//! distinct), labels — never transient ones.
//!
//! An `IStr` hashes, compares, orders, prints (`Display`, `Debug`) and
//! serializes exactly as the `String` of its text, so it replaces a `String`
//! field without changing a map's order, a key or an encoded byte.
//!
//! The table is sharded ([`SHARDS`] mutexes, picked by the text's hash): an
//! intern takes one shard's lock for a lookup and, for a new text, a copy
//! into that shard's arena chunk. An id names a slot of a table of
//! [`SEGMENTS`] (each twice the size of the one before, allocated on first
//! use), set once when the text is interned and read lock-free. Safe code
//! only: a chunk is a leaked byte slice, each text is split off its front and
//! validated back into a `str`, and a slot is a `OnceLock`.

use std::borrow::Borrow;
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

/// Shards of the table (a power of two).
const SHARDS: usize = 64;
/// Bytes of an arena chunk.
const CHUNK_BYTES: usize = 1 << 16;
/// Texts above this many bytes get an allocation of their own.
const LARGE_BYTES: usize = CHUNK_BYTES / 16;

/// An interned string: the id of its text, which lives for the rest of the
/// process. Equal texts are one id, so equality is the ids'. Cloning copies
/// the id (not `Copy`, so that a field changing from `String` keeps its
/// `clone()` calls).
#[derive(Clone)]
pub struct IStr(NonZeroU32);

/// The empty text's id (it has no slot).
const EMPTY: NonZeroU32 = NonZeroU32::MIN;
/// Segment `k` holds `2^(k + FIRST_SEGMENT_BITS)` slots; ids start at 2.
const FIRST_SEGMENT_BITS: u32 = 10;
/// Enough segments for every `u32` id.
const SEGMENTS: usize = 23;

/// The texts by id.
static SLOTS: [OnceLock<Box<[OnceLock<&'static str>]>>; SEGMENTS] =
    [const { OnceLock::new() }; SEGMENTS];
/// The next id to hand out.
static NEXT_ID: AtomicU32 = AtomicU32::new(2);

/// Entries of a thread's cache of recent interns (a power of two).
const RECENT_ENTRIES: usize = 2048;

thread_local! {
    /// A thread's recent interns by hash (direct-mapped; the empty text's
    /// hash 0 entries never match a non-empty text's comparison).
    static RECENT: std::cell::RefCell<Box<[(u64, IStr)]>> =
        std::cell::RefCell::new(vec![(0, IStr(EMPTY)); RECENT_ENTRIES].into_boxed_slice());
}

/// A hash's entry in [`RECENT`] (low bits: the shard used the middle ones).
fn recent_slot(hash: u64) -> usize {
    (hash as usize) & (RECENT_ENTRIES - 1)
}

/// The segment and the index in it of an id's slot.
fn slot_of(id: NonZeroU32) -> (usize, usize) {
    let n = u64::from(id.get()) + (1 << FIRST_SEGMENT_BITS);
    let bits = 63 - n.leading_zeros();
    let segment = (bits - FIRST_SEGMENT_BITS) as usize;
    (segment, (n - (1u64 << bits)) as usize)
}

/// An id's slot, its segment allocated on first use.
fn slot(id: NonZeroU32) -> &'static OnceLock<&'static str> {
    let (segment, index) = slot_of(id);
    let slots = SLOTS[segment].get_or_init(|| {
        (0..1usize << (segment as u32 + FIRST_SEGMENT_BITS))
            .map(|_| OnceLock::new())
            .collect()
    });
    &slots[index]
}

struct Shard {
    table: hashbrown::HashTable<IStr>,
    /// The unused rest of the shard's current chunk.
    rest: &'static mut [u8],
}

static TABLE: LazyLock<[Mutex<Shard>; SHARDS]> = LazyLock::new(|| {
    std::array::from_fn(|_| {
        Mutex::new(Shard {
            table: hashbrown::HashTable::new(),
            rest: &mut [],
        })
    })
});

/// Distinct texts interned and their bytes (arena bytes).
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
        let stored: &'static str = if text.len() > LARGE_BYTES {
            Box::leak(Box::<str>::from(text))
        } else {
            if self.rest.len() < text.len() {
                self.rest = Box::leak(vec![0u8; CHUNK_BYTES].into_boxed_slice());
            }
            let rest = std::mem::take(&mut self.rest);
            let (slot, rest) = rest.split_at_mut(text.len());
            self.rest = rest;
            slot.copy_from_slice(text.as_bytes());
            let slot: &'static [u8] = slot;
            std::str::from_utf8(slot).expect("a copy of a str is UTF-8")
        };
        INTERNED.fetch_add(1, Ordering::Relaxed);
        INTERNED_BYTES.fetch_add(text.len() as u64, Ordering::Relaxed);
        // Called under the shard's lock: each text gets one id.
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let id = NonZeroU32::new(id)
            .filter(|id| id.get() < u32::MAX)
            .expect("fewer than 2^32 interned strings");
        assert!(slot(id).set(stored).is_ok(), "an id is handed out once");
        IStr(id)
    }
}

impl IStr {
    /// The interned copy of `text`.
    pub fn new(text: &str) -> IStr {
        if text.is_empty() {
            return IStr::default();
        }
        let hash = hash_text(text);
        // A thread's recent texts: most interns repeat one (the flags of a
        // package's compiles, a snapshot decode's labels) and skip the lock.
        let cached = RECENT.with(|recent| {
            let (cached_hash, cached) = recent.borrow()[recent_slot(hash)].clone();
            (cached_hash == hash && cached.as_str() == text).then_some(cached)
        });
        if let Some(cached) = cached {
            return cached;
        }
        let interned = Self::new_shared(text, hash);
        RECENT.with(|recent| recent.borrow_mut()[recent_slot(hash)] = (hash, interned.clone()));
        interned
    }

    /// [`IStr::new`] through the shared table.
    fn new_shared(text: &str, hash: u64) -> IStr {
        let mut shard = TABLE[shard_of(hash)]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(found) = shard.table.find(hash, |entry| entry.as_str() == text) {
            return found.clone();
        }
        let interned = shard.store(text);
        shard
            .table
            .insert_unique(hash, interned.clone(), |entry| hash_text(entry.as_str()));
        interned
    }

    /// The text (it lives for the rest of the process).
    pub fn as_str(&self) -> &'static str {
        if self.0 == EMPTY {
            return "";
        }
        let (segment, index) = slot_of(self.0);
        // An IStr exists only after its slot was set (Shard::store), and
        // whoever handed it to this thread synchronized with that thread.
        SLOTS[segment]
            .get()
            .and_then(|slots| slots[index].get())
            .expect("an interned string's slot is set")
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

impl Default for IStr {
    fn default() -> Self {
        IStr(EMPTY)
    }
}

// Equal texts are one id.
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
        if self == other {
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

/// Shared `Arc` copies by content ([`shared_str`], [`shared_bytes`]): equal
/// contents handed out by one table are one allocation. Where a field is an
/// `Arc<str>` or `Arc<[u8]>` already (a runfiles path, a write action's
/// content), deduplicating at creation shares what independently computed
/// equal values each allocated, without changing the field's type, order,
/// hash or encoding. The table keeps every distinct content for the rest of
/// the process, like [`IStr`]: use it only for long-lived planning data that
/// repeats (OpenROAD: 3.7 M runfiles entries name a few thousand distinct
/// paths; 19,150 writes have 6,494 distinct contents).
struct SharedTable<T: ?Sized> {
    shards: [Mutex<hashbrown::HashTable<std::sync::Arc<T>>>; SHARED_SHARDS],
}

/// Shards of a [`SharedTable`] (a power of two).
const SHARED_SHARDS: usize = 16;

impl<T: ?Sized + Eq + AsRef<[u8]>> SharedTable<T> {
    fn new() -> Self {
        SharedTable {
            shards: std::array::from_fn(|_| Mutex::new(hashbrown::HashTable::new())),
        }
    }

    fn hash(bytes: &[u8]) -> u64 {
        let mut hasher = crate::fast_hash::FxHasher::default();
        hasher.write(bytes);
        hasher.write_usize(bytes.len());
        let hash = hasher.finish();
        (hash ^ (hash >> 29)).wrapping_mul(0xbf58_476d_1ce4_e5b9)
    }

    /// The table's copy of `value`, made by `make` the first time.
    fn get(&self, value: &T, make: impl FnOnce() -> std::sync::Arc<T>) -> std::sync::Arc<T> {
        let hash = Self::hash(value.as_ref());
        let mut shard = self.shards[((hash >> 58) as usize) & (SHARED_SHARDS - 1)]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(found) = shard.find(hash, |entry| **entry == *value) {
            return std::sync::Arc::clone(found);
        }
        let shared = make();
        shard.insert_unique(hash, std::sync::Arc::clone(&shared), |entry| {
            Self::hash((**entry).as_ref())
        });
        shared
    }
}

static SHARED_STRS: LazyLock<SharedTable<str>> = LazyLock::new(SharedTable::new);
static SHARED_BYTES: LazyLock<SharedTable<[u8]>> = LazyLock::new(SharedTable::new);

/// The process's shared `Arc<str>` of `text` ([`SharedTable`]).
pub fn shared_str(text: &str) -> std::sync::Arc<str> {
    SHARED_STRS.get(text, || std::sync::Arc::from(text))
}

/// The process's shared `Arc<[u8]>` of `bytes` ([`SharedTable`]).
pub fn shared_bytes(bytes: &[u8]) -> std::sync::Arc<[u8]> {
    SHARED_BYTES.get(bytes, || std::sync::Arc::from(bytes))
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
            assert_eq!(format!("{interned}"), string);
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
    fn shared_arcs_are_one_allocation_per_content() {
        use super::{shared_bytes, shared_str};
        let a = shared_str(&format!("{}/{}", "_main", "tool.sh"));
        let b = shared_str("_main/tool.sh");
        assert!(std::sync::Arc::ptr_eq(&a, &b));
        assert_eq!(&*a, "_main/tool.sh");
        assert!(!std::sync::Arc::ptr_eq(&a, &shared_str("_main/other.sh")));
        let c = shared_bytes(b"content\0");
        let copy: Vec<u8> = b"content\0".into();
        assert!(std::sync::Arc::ptr_eq(&c, &shared_bytes(&copy)));
        assert_eq!(&*shared_bytes(b""), b"");
    }

    #[test]
    fn ids_map_to_distinct_slots() {
        use super::{FIRST_SEGMENT_BITS, slot_of};
        use std::num::NonZeroU32;
        let id = |n: u32| NonZeroU32::new(n).unwrap();
        assert_eq!(slot_of(id(2)), (0, 2));
        let first = (1u32 << FIRST_SEGMENT_BITS) - 1;
        assert_eq!(slot_of(id(first)), (0, first as usize));
        assert_eq!(slot_of(id(first + 1)), (1, 0));
        assert_eq!(slot_of(id(u32::MAX)).0, super::SEGMENTS - 1);
        let mut previous = slot_of(id(2));
        for n in 3..100_000 {
            let next = slot_of(id(n));
            assert!(next == (previous.0, previous.1 + 1) || next == (previous.0 + 1, 0));
            previous = next;
        }
        assert_eq!(std::mem::size_of::<IStr>(), 4);
        assert_eq!(std::mem::size_of::<Option<IStr>>(), 4);
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
