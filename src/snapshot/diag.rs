//! The diagnostics stream (PLAN-L2 §1d "Capturing what planning printed").
//!
//! While crate::snapshot::capture redirects file descriptor 2 into its pipe
//! (the analysis snapshot's recording window, or a hit attempt), everything
//! written to stderr becomes part of what planning printed — the bytes a
//! hit replays. Diagnostics must not: their content depends on the run
//! (timings, memory, the snapshot's own report), so the PERF marks
//! (crate::perf_mark), the stage report (crate::plan_timings), the memory
//! watchdog (crate::memory_limit), the snapshot diagnostics
//! (`bazel-rs-snapshot:` lines, crate::observed_fs::diag) and the codec's
//! `PERF-PLAN` line write here instead: to the original stderr while a
//! capture is active, to stderr otherwise. Each call writes whole lines in
//! one `write`, so a diagnostics line never splits.

use std::fmt;
use std::fs::File;
use std::io::Write;
use std::sync::{Arc, PoisonError, RwLock};

/// The original stderr while a capture holds file descriptor 2.
static ORIGINAL: RwLock<Option<Arc<File>>> = RwLock::new(None);

/// crate::snapshot::capture: route diagnostics to `original` (the saved
/// stderr) while it redirects file descriptor 2, back to fd 2 with `None`.
pub(crate) fn set_original(original: Option<Arc<File>>) {
    *ORIGINAL.write().unwrap_or_else(PoisonError::into_inner) = original;
}

/// Write `bytes` (whole lines) to the diagnostics stream.
pub fn write(bytes: &[u8]) {
    let original = ORIGINAL
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    match original {
        Some(file) => {
            let _ = (&*file).write_all(bytes);
        }
        None => {
            let _ = std::io::stderr().lock().write_all(bytes);
        }
    }
}

/// One diagnostics line (`text` without its newline).
pub fn line(text: &str) {
    let mut bytes = Vec::with_capacity(text.len() + 1);
    bytes.extend_from_slice(text.as_bytes());
    bytes.push(b'\n');
    write(&bytes);
}

/// [`line`] of formatted text (`diag::line_fmt(format_args!(...))`).
pub fn line_fmt(arguments: fmt::Arguments<'_>) {
    line(&arguments.to_string());
}
