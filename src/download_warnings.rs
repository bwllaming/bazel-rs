//! The `WARNING: Download from URL failed` lines of speculative transfers
//! nobody committed (docs/COLD_ANALYSIS.md "Module extensions evaluated
//! ahead", cold-eval review): a speculation (a repository's, or a module
//! extension evaluated ahead) that downloads a checksummed blob after a
//! failed mirror stores it in the download cache, and its held output —
//! those warnings — is dropped when it is retried, blocked, failed or not
//! kept. The sequential engine's own download of that blob would then be a
//! cache hit and print nothing, where it would have printed the warnings.
//! So the warnings of an uncommitted speculation are kept by cache key for
//! the invocation, and the next cache hit of that key writes them where
//! the download would have.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};

/// The cache key of a checksummed download.
pub(crate) fn key(checksum: &crate::extension_download::Checksum) -> String {
    format!(
        "{}:{}",
        checksum.algorithm.name(),
        crate::extension_download::hex(&checksum.bytes)
    )
}

thread_local! {
    /// The speculation running on this thread: its transfers' warnings by
    /// cache key, in order.
    static COLLECTOR: RefCell<Option<Vec<(String, String)>>> = const { RefCell::new(None) };
}

/// Uncommitted speculations' warnings, by cache key.
static PENDING: LazyLock<Mutex<HashMap<String, Vec<String>>>> = LazyLock::new(Mutex::default);

/// What one speculation's transfers warned ([`collect`]).
#[derive(Debug, Default)]
pub(crate) struct Held(Vec<(String, String)>);

impl Held {
    /// The speculation is not committed: its warnings are written at the
    /// next cache hit of their keys.
    pub(crate) fn release(self) {
        if self.0.is_empty() {
            return;
        }
        let mut pending = PENDING.lock().unwrap_or_else(PoisonError::into_inner);
        for (key, warning) in self.0 {
            pending.entry(key).or_default().push(warning);
        }
    }
}

/// Run `work` (a speculation) collecting what its transfers warn.
pub(crate) fn collect<R>(work: impl FnOnce() -> R) -> (R, Held) {
    let previous = COLLECTOR.with(|collector| collector.replace(Some(Vec::new())));
    let result = work();
    let held = COLLECTOR
        .with(|collector| collector.replace(previous))
        .unwrap_or_default();
    (result, Held(held))
}

/// A failed mirror of a download expecting `expected` warned `warning`
/// (written or held as usual by the caller).
pub(crate) fn note(expected: Option<&crate::extension_download::Checksum>, warning: &str) {
    let Some(expected) = expected else {
        return;
    };
    COLLECTOR.with(|collector| {
        if let Some(collector) = collector.borrow_mut().as_mut() {
            collector.push((key(expected), warning.to_owned()));
        }
    });
}

/// A cache hit of `expected`: the warnings an uncommitted speculation's
/// transfer of it printed (once).
pub(crate) fn take(expected: &crate::extension_download::Checksum) -> Vec<String> {
    let mut pending = PENDING.lock().unwrap_or_else(PoisonError::into_inner);
    if pending.is_empty() {
        return Vec::new();
    }
    pending.remove(&key(expected)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension_download::{Algorithm, Checksum};

    #[test]
    fn released_warnings_replay_once_at_the_next_hit() {
        let checksum = Checksum {
            algorithm: Algorithm::Sha256,
            bytes: vec![0xab; 32],
        };
        let other = Checksum {
            algorithm: Algorithm::Sha256,
            bytes: vec![0xcd; 32],
        };
        let ((), held) = collect(|| {
            note(Some(&checksum), "WARNING: one");
            note(None, "WARNING: no key");
            note(Some(&checksum), "WARNING: two");
        });
        assert!(take(&checksum).is_empty(), "nothing before the release");
        held.release();
        assert!(take(&other).is_empty());
        assert_eq!(take(&checksum), ["WARNING: one", "WARNING: two"]);
        assert!(take(&checksum).is_empty());
        // A committed speculation's warnings are dropped with it.
        let ((), held) = collect(|| note(Some(&other), "WARNING: committed"));
        drop(held);
        assert!(take(&other).is_empty());
    }
}
