//! A fast, non-cryptographic hasher for the executor's per-process sets and
//! maps of paths (the FxHash function of rustc: a multiply and a rotate per
//! word). The standard SipHash costs about as much as the rest of a warm
//! no-op's per-input work; these keys are paths the build itself produced,
//! never attacker-chosen input whose collisions could matter, and a
//! collision only costs a comparison.
//!
//! Keyed by bytes (`&[u8]` of a path), never by `Path`: `Path`'s `Hash`
//! parses components byte by byte. Callers key only paths that are spelled
//! as their components (crate::action::validate_relative), where byte
//! equality is path equality.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

/// The FxHash hasher.
#[derive(Default, Clone, Copy)]
pub struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for word in words {
            self.add(u64::from_le_bytes(*word));
        }
        if !rest.is_empty() {
            let mut word = [0u8; 8];
            word[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(word));
        }
    }

    #[inline]
    fn write_u8(&mut self, value: u8) {
        self.add(u64::from(value));
    }

    #[inline]
    fn write_u32(&mut self, value: u32) {
        self.add(u64::from(value));
    }

    #[inline]
    fn write_u64(&mut self, value: u64) {
        self.add(value);
    }

    #[inline]
    fn write_usize(&mut self, value: usize) {
        self.add(value as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

pub type FxBuildHasher = BuildHasherDefault<FxHasher>;
pub type FxHashSet<T> = HashSet<T, FxBuildHasher>;
pub type FxHashMap<K, V> = HashMap<K, V, FxBuildHasher>;

/// An empty set with room for `capacity` entries.
pub fn set_with_capacity<T>(capacity: usize) -> FxHashSet<T> {
    HashSet::with_capacity_and_hasher(capacity, FxBuildHasher::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_bytes_hash_equal_and_lengths_matter() {
        let hash = |bytes: &[u8]| {
            let mut hasher = FxHasher::default();
            hasher.write(bytes);
            hasher.finish()
        };
        assert_eq!(hash(b"a/b/c"), hash(b"a/b/c"));
        assert_ne!(hash(b"a/b/c"), hash(b"a/b/d"));
        let mut set: FxHashSet<&[u8]> = set_with_capacity(4);
        assert!(set.insert(b"a/b"));
        assert!(!set.insert(b"a/b"));
        assert!(set.insert(b"a/b\0"));
        assert!(set.contains(&b"a/b"[..]));
    }
}
