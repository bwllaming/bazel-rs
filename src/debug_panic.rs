//! `BAZEL_RS_DEBUG_PANIC_AT=<site>[:<n>]` (hidden test hook): the `n`th
//! (default first) pass through the named site panics, to check that an
//! internal error on any thread ends the process with exit code 37
//! promptly (crate::internal_error), never a hang. Sites: `coordinator`
//! (the walk's coordinator, mid-walk), `analysis-worker`, `lowering`,
//! `drop` (the planning drop thread), `evaluator` (an evaluator pool
//! thread, BAZEL_RS_ANALYSIS=eval), `loading-worker`.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// The configured site and pass number (`None`: the hook is off).
fn spec() -> Option<&'static (String, u64)> {
    static SPEC: OnceLock<Option<(String, u64)>> = OnceLock::new();
    SPEC.get_or_init(|| {
        // snapshot-unrecorded: a hidden test hook that panics on purpose; never set outside the tests.
        let value = std::env::var("BAZEL_RS_DEBUG_PANIC_AT").ok()?;
        let (site, pass) = match value.split_once(':') {
            Some((site, pass)) => (site.to_owned(), pass.parse().ok()?),
            None => (value, 1),
        };
        Some((site, pass))
    })
    .as_ref()
}

/// Panic here when the hook names `site` and this is its configured pass.
pub fn at(site: &str) {
    static PASSES: AtomicU64 = AtomicU64::new(0);
    let Some((wanted, pass)) = spec() else {
        return;
    };
    if wanted == site && PASSES.fetch_add(1, Ordering::Relaxed) + 1 == *pass {
        panic!("BAZEL_RS_DEBUG_PANIC_AT: {site}");
    }
}
