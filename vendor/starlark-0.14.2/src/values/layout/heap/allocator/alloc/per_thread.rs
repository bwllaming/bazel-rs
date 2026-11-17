/*
 * Copyright 2019 The Starlark in Rust Authors.
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

// Modified by bazel-rs: the next chunk's size follows the bytes the bump holds,
// not its chunk count (no shift overflow at 32 chunks); see vendor provenance.

use std::cell::RefCell;
use std::cmp;
use std::mem;

use crate::values::layout::aligned_size::AlignedSize;
use crate::values::layout::heap::allocator::alloc::chain::ChunkChain;
use crate::values::layout::heap::allocator::alloc::chunk::Chunk;
use crate::values::layout::heap::allocator::alloc::chunk_part::ChunkPart;
use crate::values::layout::heap::arena::MIN_ALLOC;

/// Minimum usable cached allocation.
const MIN_USABLE_ALLOC: AlignedSize = AlignedSize::new_bytes(
    (
        // * All chunks are used to store chains, so we need to include chain header.
        // * We need to be able to store at least one object after the header.
        ChunkChain::HEADER_SIZE.bytes() + MIN_ALLOC.bytes()
    ) as usize,
);

#[derive(Debug, Default)]
struct PerThreadChunkCache {
    /// Keep a few last chunks.
    ///
    /// Frozen heap has two arenas: drop and non-drop. So we should keep at least two chunks.
    last_chunks: [ChunkPart; 4],
}

impl PerThreadChunkCache {
    /// Save a chunk to the thread-local cache if it is large enough.
    fn store(&mut self, mut chunk: ChunkPart) {
        for next in &mut self.last_chunks {
            // Keep the largest chunks in the pool.
            if chunk.len() > next.len() {
                mem::swap(next, &mut chunk);
            }
        }
    }

    /// Fetch a chunk from the thread-local cache if the cache has a chunk large enough.
    fn fetch(&mut self, len: AlignedSize) -> Option<ChunkPart> {
        for next in &mut self.last_chunks {
            // Pick any chunk which is large enough.
            if next.len() >= len {
                let result = mem::take(next);
                return Some(result);
            }
        }
        None
    }
}

thread_local! {
    static PER_THREAD_ALLOCATOR: RefCell<PerThreadChunkCache> = RefCell::new(PerThreadChunkCache::default());
}

/// The first chunk of a bump.
const FIRST_CHUNK_BYTES: usize = 512;
/// No chunk is larger than this unless one value needs more.
const MAX_CHUNK_BYTES: usize = 1 << 31;

/// Size of the next chunk of a bump whose chunks so far hold `allocated_in_bump` bytes.
///
/// Replicates `bumpalo`: 512 in the first chunk, and each next chunk about doubles
/// the bump, but not greater than 2G. For a chain of whole chunks this is exactly
/// the 512, 1K, 2K, ... sequence.
///
/// bazel-rs: the size follows the bytes the bump holds, not its chunk count
/// (upstream computed `512u32.checked_shl(chunk_count).unwrap()`). Chunks fetched
/// from the thread-local cache are small fragments of other heaps' chunks, so a
/// bump that mostly reuses fragments can have any number of chunks while holding
/// little memory: the shift panicked at 32 chunks (OSS corpus capnproto, the frozen
/// heap of a C++ rule's analysis), and from 22 chunks on every new chunk was 2G.
fn next_chunk_size(allocated_in_bump: usize) -> AlignedSize {
    let bytes = allocated_in_bump
        .saturating_add(FIRST_CHUNK_BYTES)
        .checked_next_power_of_two()
        .unwrap_or(MAX_CHUNK_BYTES)
        .min(MAX_CHUNK_BYTES);
    AlignedSize::new_bytes(bytes)
}

/// Allocate chunk which is large enough for given number of words.
///
/// `allocated_in_bump` is the number of bytes in the chunks the bump already has.
pub(crate) fn thread_local_alloc_at_least(len: AlignedSize, allocated_in_bump: usize) -> ChunkPart {
    let chunk = match PER_THREAD_ALLOCATOR.with_borrow_mut(|allocator| allocator.fetch(len)) {
        Some(chunk) => chunk,
        _ => {
            let next_chunk_size = next_chunk_size(allocated_in_bump) - Chunk::HEADER_SIZE;
            let len = cmp::max(len, next_chunk_size);
            ChunkPart::alloc_at_least(len)
        }
    };
    debug_assert!(chunk.len() >= len);
    chunk
}

/// Release chunk part to thread-local pool.
#[allow(clippy::if_same_then_else)]
#[inline]
pub(crate) fn thread_local_release(chunk: ChunkPart) {
    if chunk.is_full() {
        // Chunk part is the full chunk. Better return it to malloc.
        drop(chunk)
    } else if chunk.len() < MIN_USABLE_ALLOC {
        // It is not reusable.
        drop(chunk)
    } else if chunk.chunk_ref_count() == 1 {
        // We could reuse the chunk, but since it is not shared,
        // better return it to malloc.
        drop(chunk);
    } else {
        PER_THREAD_ALLOCATOR.with_borrow_mut(|allocator| allocator.store(chunk));
    }
}

#[cfg(test)]
mod tests {
    use crate::values::layout::aligned_size::AlignedSize;
    use crate::values::layout::heap::allocator::alloc::allocator::ChunkAllocator;
    use crate::values::layout::heap::allocator::alloc::chain::ChunkChain;
    use crate::values::layout::heap::allocator::alloc::chunk::Chunk;
    use crate::values::layout::heap::allocator::alloc::chunk_part::ChunkPart;
    use crate::values::layout::heap::allocator::alloc::per_thread::MAX_CHUNK_BYTES;
    use crate::values::layout::heap::allocator::alloc::per_thread::PER_THREAD_ALLOCATOR;
    use crate::values::layout::heap::allocator::alloc::per_thread::PerThreadChunkCache;
    use crate::values::layout::heap::allocator::alloc::per_thread::next_chunk_size;
    use crate::values::layout::heap::allocator::api::ArenaAllocator;
    use crate::values::layout::heap::repr::AValueHeader;
    use crate::values::layout::value_alloc_size::ValueAllocSize;

    #[test]
    fn test_next_chunk_size_doubles_up_to_the_cap() {
        assert_eq!(512, next_chunk_size(0).bytes());
        // A chain of whole chunks follows 512, 1K, 2K, ...: each chunk holds a bit
        // less than its size (headers), the sum of the earlier ones is under the next.
        let mut allocated = 0;
        for shift in 0..22 {
            let size = next_chunk_size(allocated).bytes() as usize;
            assert_eq!(512 << shift, size);
            allocated += size - (Chunk::HEADER_SIZE + ChunkChain::HEADER_SIZE).bytes() as usize;
        }
        for allocated in [
            MAX_CHUNK_BYTES - 1,
            MAX_CHUNK_BYTES,
            usize::MAX / 2,
            usize::MAX,
        ] {
            assert_eq!(MAX_CHUNK_BYTES, next_chunk_size(allocated).bytes() as usize);
        }
    }

    /// A bump that takes many small fragments from the thread-local cache gets
    /// more than 32 chunks; the next fresh chunk must neither panic (upstream
    /// shifted `512u32` by the chunk count) nor be 2G (from 22 chunks on).
    #[test]
    fn test_bump_of_many_cached_fragments() {
        PER_THREAD_ALLOCATOR.with_borrow_mut(|cache| *cache = PerThreadChunkCache::default());
        let allocator = ChunkAllocator::default();
        let fragment_len =
            ChunkChain::HEADER_SIZE + AlignedSize::new_bytes(8 * AValueHeader::ALIGN);
        // The other halves keep the fragments' chunks shared, as live heaps do.
        let mut other_halves = Vec::new();
        for _ in 0..40 {
            let chunk = ChunkPart::alloc_at_least(fragment_len + fragment_len);
            let (fragment, rest) = chunk.split_at_offset(fragment_len);
            PER_THREAD_ALLOCATOR.with_borrow_mut(|cache| cache.store(fragment));
            other_halves.push(rest);
            // Fills the fragment exactly, so the next allocation needs a new chunk.
            allocator.alloc(ValueAllocSize::new(fragment_len - ChunkChain::HEADER_SIZE));
            assert_eq!(0, allocator.remaining_capacity());
        }
        let chunks = unsafe { allocator.iter_allocated_chunks_rev().count() };
        assert_eq!(40, chunks);
        allocator.alloc(ValueAllocSize::new(AlignedSize::new_bytes(
            2 * AValueHeader::ALIGN,
        )));
        let chunks = unsafe { allocator.iter_allocated_chunks_rev().count() };
        assert_eq!(41, chunks);
        // 40 fragments of 8 words: the new chunk is a few KiB.
        assert!(allocator.remaining_capacity() < 8 << 10);
    }

    #[test]
    fn test_release_partial() {
        let mut allocator = PerThreadChunkCache::default();
        let chunk = ChunkPart::alloc_at_least(AlignedSize::new_bytes(10 * AValueHeader::ALIGN));
        let (a, b) = chunk.split_at_offset(AlignedSize::new_bytes(5 * AValueHeader::ALIGN));
        let old_a_ptr = a.begin().as_ptr();
        let old_b_ptr = b.begin().as_ptr();
        allocator.store(a);
        allocator.store(b);
        let a = allocator
            .fetch(AlignedSize::new_bytes(3 * AValueHeader::ALIGN))
            .unwrap();
        let b = allocator
            .fetch(AlignedSize::new_bytes(3 * AValueHeader::ALIGN))
            .unwrap();
        assert!(
            std::ptr::eq(old_a_ptr, a.begin().as_ptr())
                || std::ptr::eq(old_a_ptr, b.begin().as_ptr())
        );
        assert!(
            std::ptr::eq(old_b_ptr, a.begin().as_ptr())
                || std::ptr::eq(old_b_ptr, b.begin().as_ptr())
        );
    }
}
