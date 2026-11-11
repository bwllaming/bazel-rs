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

/// Whether `BAZEL_RS_NO_ARENA_BINDING=1` (hidden) keeps every worker in its
/// own arena: a diagnostics switch to measure arena-lock contention.
pub fn binding_disabled() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| {
        // snapshot-unrecorded: a diagnostics switch choosing where memory is allocated; no result depends on it.
        std::env::var_os("BAZEL_RS_NO_ARENA_BINDING").is_some_and(|value| value == "1")
    })
}

/// The arena the calling thread allocates in, to bind workers to (`None`
/// without jemalloc, or with the binding switched off).
pub fn current() -> Option<u32> {
    if binding_disabled() {
        return None;
    }
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

/// Write a jemalloc heap profile to `path` (`prof.dump`; a no-op unless
/// the binary is built with `tikv-jemallocator/profiling` and runs with
/// `_RJEM_MALLOC_CONF=prof:true`): docs/MEMORY.md, `BAZEL_RS_TIMING_HEAP_DUMP`.
pub fn heap_dump(path: &std::path::Path) {
    #[cfg(feature = "jemalloc")]
    {
        use std::os::unix::ffi::OsStrExt;
        let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return;
        };
        let mut pointer = path.as_ptr();
        // SAFETY: `prof.dump` takes a `const char *` file name; we pass a
        // pointer to a valid pointer to a NUL-terminated string that
        // outlives the call, and read nothing back.
        unsafe {
            tikv_jemalloc_sys::mallctl(
                c"prof.dump".as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                (&raw mut pointer).cast(),
                std::mem::size_of::<*const std::ffi::c_char>(),
            );
        }
    }
    #[cfg(not(feature = "jemalloc"))]
    let _ = path;
}

/// jemalloc's `stats.{allocated, active, resident, retained}` in bytes
/// (refreshed; `None` without jemalloc): diagnostics,
/// `BAZEL_RS_TIMING_JEMALLOC`.
pub fn stats() -> Option<[usize; 4]> {
    #[cfg(feature = "jemalloc")]
    {
        let mut epoch: u64 = 1;
        let mut length = std::mem::size_of::<u64>();
        // SAFETY: `epoch` is a read/write `uint64_t`; we pass valid pointers
        // and sizes.
        unsafe {
            tikv_jemalloc_sys::mallctl(
                c"epoch".as_ptr(),
                (&raw mut epoch).cast(),
                &raw mut length,
                (&raw mut epoch).cast(),
                std::mem::size_of::<u64>(),
            );
        }
        let read = |name: &std::ffi::CStr| {
            let mut value: usize = 0;
            let mut length = std::mem::size_of::<usize>();
            // SAFETY: the `stats.*` controls are read-only `size_t`s; we
            // pass a valid pointer to a `usize` and its size.
            let status = unsafe {
                tikv_jemalloc_sys::mallctl(
                    name.as_ptr(),
                    (&raw mut value).cast(),
                    &raw mut length,
                    std::ptr::null_mut(),
                    0,
                )
            };
            (status == 0).then_some(value)
        };
        Some([
            read(c"stats.allocated")?,
            read(c"stats.active")?,
            read(c"stats.resident")?,
            read(c"stats.retained")?,
        ])
    }
    #[cfg(not(feature = "jemalloc"))]
    None
}

/// Return every arena's dirty pages to the system now (`arena.<all>.purge`):
/// called right after a phase frees large structures, so the next phase's
/// peak does not stack on memory that would otherwise wait for its decay
/// (docs/MEMORY.md, "The allocator"). A no-op without jemalloc.
pub fn purge() {
    #[cfg(feature = "jemalloc")]
    {
        // MALLCTL_ARENAS_ALL (jemalloc.h): every arena.
        // SAFETY: `arena.<i>.purge` is a void control taking and returning
        // nothing; we pass null pointers and zero sizes.
        unsafe {
            tikv_jemalloc_sys::mallctl(
                c"arena.4096.purge".as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_thread_binds_to_another_threads_arena() {
        if super::binding_disabled() {
            return;
        }
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
