//! The exit code of a failed command, by the failure's class (Bazel's
//! ExitCode, util/ExitCode.java, from the failure's FailureDetail; genuine
//! matrix in tests/fixtures/exit_codes).
//!
//! Most native failures travel as an error text to crate::main, which exits
//! 1 (BUILD_FAILURE / PARSING_FAILURE). A failure of another class records
//! its code here where it is detected (the first recorded one wins); main
//! exits with it. Codes with their own exit paths: 3 (tests failed), 4 (no
//! test targets), 7 (query's `ANALYSIS_FAILURE`-class loading errors), 8
//! (interrupted, crate::signals; `fetch --repo` failures), 33 (out of
//! memory, crate::memory_limit), 34 (a remote failure,
//! crate::remote_cache::note_remote_failure), 36 (a sandbox setup failure,
//! crate::sandbox), 32 (a registry that module resolution could not read,
//! [`main_repository_mapping_failure`]), 37 (`mod`'s invalid subcommand). A
//! recorded code wins over 34 and 36 (crate::main's failure_exit_code).

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// ExitCode.COMMAND_LINE_ERROR: an option the command does not accept, an
/// option value that does not parse, an undefined `--config`, a `run` target
/// that cannot run.
pub const COMMAND_LINE_ERROR: i32 = 2;
/// ExitCode.REMOTE_ENVIRONMENTAL_ERROR: ExternalDeps.ERROR_ACCESSING_REGISTRY
/// (a registry file that could not be fetched).
pub const REMOTE_ENVIRONMENTAL_ERROR: i32 = 32;
static RECORDED: AtomicI32 = AtomicI32::new(0);
static REGISTRY_ACCESS_FAILED: AtomicBool = AtomicBool::new(false);

/// Module resolution could not fetch a registry file (ModuleFileFunction:
/// `Error accessing registry REGISTRY: …`, ERROR_ACCESSING_REGISTRY).
pub fn note_registry_access_failure() {
    REGISTRY_ACCESS_FAILED.store(true, Ordering::Relaxed);
}

/// The ERROR message of a command that failed because module resolution
/// could not access a registry: Bazel computes the main repository mapping
/// before loading anything, so the failure surfaces as
/// `Error computing the main repository mapping: Error accessing registry …`
/// (RepositoryMappingValue.RepositoryMappingResolutionException), exit
/// [`REMOTE_ENVIRONMENTAL_ERROR`]. `None` for any other failure (also one
/// that only wraps such an error). The typed flag gates the text.
pub fn main_repository_mapping_failure(error: &str) -> Option<String> {
    (REGISTRY_ACCESS_FAILED.load(Ordering::Relaxed)
        && error.starts_with("Error accessing registry "))
    .then(|| format!("Error computing the main repository mapping: {error}"))
}

/// Record the exit code of this command's failure (the first one wins).
pub fn record(code: i32) {
    let _ = RECORDED.compare_exchange(0, code, Ordering::Relaxed, Ordering::Relaxed);
}

/// The recorded exit code, if any.
pub fn recorded() -> Option<i32> {
    match RECORDED.load(Ordering::Relaxed) {
        0 => None,
        code => Some(code),
    }
}

/// `error` as a command-line error ([`COMMAND_LINE_ERROR`]), unless it is
/// a [`native_limitation`].
pub fn command_line(error: String) -> String {
    if !native_limitation(&error) {
        record(COMMAND_LINE_ERROR);
    }
    error
}

/// Whether an option error is the native profile's own rejection of an
/// option Bazel accepts (`unsupported option …`, `unsupported startup
/// option …`, `…; options are never
/// silently ignored`, `… is not implemented in the native profile`): a native
/// failure, exit 1, not a command-line error Bazel would report.
pub fn native_limitation(error: &str) -> bool {
    error.starts_with("unsupported ")
        || error.contains("options are never silently ignored")
        || error.contains("not implemented in the native profile")
}
