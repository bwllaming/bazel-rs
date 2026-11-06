//! The calling thread's jemalloc arena (`thread.arena`), read and bound
//! through jemalloc's documented `mallctl` (tikv-jemallocator has no safe
//! API for it). Without the "jemalloc" feature (the system allocator)
//! there is no arena: `current` is `None` and `bind` does nothing.
//!
//! Workers that hand most of what they allocate to another thread (the
//! lowering workers: their results are placed and later freed by the
//! coordinator) bind to that thread's arena, so what they free is reusable
//! there at once instead of waiting in an arena of their own until it
//! decays (docs/MEMORY.md, "The allocator").
#![allow(unsafe_code)]

/// The arena the calling thread allocates in.
pub fn current() -> Option<u32> {
    #[cfg(feature = "jemalloc")]
    {
        let mut arena: u32 = 0;
        let mut length = std::mem::size_of::<u32>();
        // SAFETY: `thread.arena` is an `unsigned` read/write control; we
        // pass a valid pointer to a `u32` and its size, and no new value.
        let status = unsafe {
            tikv_jemalloc_sys::mallctl(
                c"thread.arena".as_ptr(),
                (&raw mut arena).cast(),
                &raw mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        (status == 0 && length == std::mem::size_of::<u32>()).then_some(arena)
    }
    #[cfg(not(feature = "jemalloc"))]
    None
}

/// Allocate in `arena` (an index `current` returned) from now on; whether
/// jemalloc accepted it.
pub fn bind(arena: u32) -> bool {
    #[cfg(feature = "jemalloc")]
    {
        let mut arena = arena;
        // SAFETY: `thread.arena` takes an `unsigned`; we pass a valid
        // pointer to a `u32` and its size, and read nothing back. The index
        // came from `thread.arena`, so the arena exists (arenas are never
        // destroyed here).
        let status = unsafe {
            tikv_jemalloc_sys::mallctl(
                c"thread.arena".as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                (&raw mut arena).cast(),
                std::mem::size_of::<u32>(),
            )
        };
        status == 0
    }
    #[cfg(not(feature = "jemalloc"))]
    {
        let _ = arena;
        false
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_thread_binds_to_another_threads_arena() {
        let Some(arena) = super::current() else {
            return;
        };
        let seen = std::thread::spawn(move || {
            assert!(super::bind(arena));
            super::current()
        })
        .join()
        .unwrap();
        assert_eq!(seen, Some(arena));
    }
}
