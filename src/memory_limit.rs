//! Process memory ceiling (native-only safety valve, watchdog-only).
//!
//! `BAZEL_RS_MAX_MEMORY_MB` (environment) or `--memory-limit=<MB>`
//! (build/run/test/query option; flag wins over env, Bazel's CLI-over-env
//! convention) caps THIS process's RSS. When neither is set, the cap
//! defaults to the smaller of 16 GB and half of MemTotal (/proc/meminfo)
//! — see [`default_cap_for_total_mb`] — so a runaway invocation fails
//! gracefully instead of OOM-killing the machine or starving its other
//! services. Set the env var to move the
//! default: the env var is the recommended place — `--memory-limit` in
//! a shared `.bazelrc` also reaches genuine Bazel, which rejects the
//! unknown option and breaks.
//!
//! There is deliberately NO RLIMIT_AS (perf follow-up 2): an address-
//! space rlimit is inherited by every child process, so a cap sized for
//! bazel-rs would hard-break legitimate child actions (node/JVM/Go
//! reserve far more virtual space than they touch). The watchdog caps
//! bazel-rs itself, not child actions; children keep the host's limits.
//!
//! A watchdog thread polls `/proc/self/statm` every 250 ms. RSS over
//! 75% of the cap warns ONCE; RSS over the cap sets the stop flag,
//! SIGKILLs every live child process group registered through
//! `crate::spawn_lock::spawn` or, for a supervised spawn, the command's
//! group its supervisor reported (crate::process_wrapper; the supervisor
//! itself then kills what the command left, and every supervisor kills
//! its command when this process exits) (reusing `crate::git_process::kill_group`),
//! prints `bazel-rs: memory limit of {cap} MB exceeded (killed)` and
//! exits with code 33 — Bazel's own OOM exit code
//! (ExitCode.java: OUT_OF_MEMORY = 33), so callers that special-case a
//! Bazel OOM keep working. Rust allocation failure itself aborts
//! (handle_alloc_error) and cannot be mapped gracefully; the watchdog
//! polling interval bounds how far past the cap RSS can grow between
//! checks. Caps below 256 MB are rejected with a clear message: the
//! binary's own startup footprint makes them unsatisfiable.

use std::sync::atomic::{AtomicBool, Ordering};

/// Exit code when the watchdog kills the process for exceeding the cap:
/// Bazel's OUT_OF_MEMORY (ExitCode.java:33), not a private code.
pub const LIMIT_EXIT_CODE: i32 = 33;

/// Smallest accepted cap; below this the process cannot even start.
pub const MIN_CAP_MB: u64 = 256;

static WATCHDOG_STARTED: AtomicBool = AtomicBool::new(false);
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
/// The installed cap is the silent default (set by main before install).
static DEFAULT_CAP_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Mark the cap about to be installed as the default (main only, so
/// report_peak can stay silent for it).
pub fn mark_default_cap() {
    DEFAULT_CAP_ACTIVE.store(true, Ordering::SeqCst);
}
static LIVE_GROUPS: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

/// True once the watchdog has fired: workers should stop starting new
/// work (the process exits immediately after the kill sweep; this flag
/// only narrows the window in which a worker could spawn a child whose
/// group the sweep already passed).
pub fn stop_requested() -> bool {
    STOP_REQUESTED.load(Ordering::SeqCst)
}

/// Whether the watchdog is running (a cap is installed). The spawn path
/// consults this before doing the /proc group-leader check.
pub fn watchdog_armed() -> bool {
    WATCHDOG_STARTED.load(Ordering::SeqCst)
}

/// Register a live child process group for the watchdog kill sweep.
/// Called by `crate::spawn_lock::spawn` for every child started in its
/// own process group while the watchdog is armed. Groups whose leader
/// already exited stay listed until exit: kill_group treats ESRCH (no
/// member left) as success, and group ids are not reused while any
/// member lives, so a stale entry can only address a group that has a
/// living member descended from (or equal to) the registered child.
pub fn register_live_group(group: u32) {
    if let Ok(mut groups) = LIVE_GROUPS.lock()
        && !groups.contains(&group)
    {
        groups.push(group);
    }
}

/// Drop a registered group once its spawn has ended (a supervised spawn,
/// crate::process_wrapper: the supervisor has killed and reaped every
/// process of it, so the ID may be reused and must not be swept later).
pub fn unregister_live_group(group: u32) {
    if let Ok(mut groups) = LIVE_GROUPS.lock() {
        groups.retain(|registered| *registered != group);
    }
}

/// Snapshot of the registered live groups (watchdog sweep + tests).
pub fn live_groups() -> Vec<u32> {
    LIVE_GROUPS
        .lock()
        .map(|groups| groups.clone())
        .unwrap_or_default()
}

/// Kill every registered live group (SIGKILL via kill_group, ESRCH =
/// already gone). Best-effort: a failed kill never blocks the exit.
fn kill_live_groups() {
    for group in live_groups() {
        let _ = crate::git_process::kill_group(group);
    }
}

/// Stop new spawns and kill every registered live group, for an exit on
/// an internal error (crate::internal_error): never waits for the groups'
/// lock (the panicking thread may hold it).
pub fn kill_live_children() {
    STOP_REQUESTED.store(true, Ordering::SeqCst);
    let groups = match LIVE_GROUPS.try_lock() {
        Ok(groups) => groups.clone(),
        Err(std::sync::TryLockError::Poisoned(groups)) => groups.into_inner().clone(),
        Err(std::sync::TryLockError::WouldBlock) => return,
    };
    for group in groups {
        let _ = crate::git_process::kill_group(group);
    }
}

/// Parse a cap in MB from a user value. `None` = unset/invalid (invalid
/// values warn at the call site; empty means unset).
pub fn parse_cap_mb(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    match value.parse::<u64>() {
        Ok(mb) if mb > 0 => Some(mb),
        _ => None,
    }
}

/// Human-readable default policy, interpolated into the below-minimum
/// error so the two never drift apart.
pub const DEFAULT_POLICY: &str = "the default cap (the smaller of 16 GB and 50% of machine memory)";

/// Validate an explicit cap: below MIN_CAP_MB the process's own startup
/// footprint exceeds the budget, so the cap could only ever kill.
pub fn validate_cap_mb(cap_mb: u64) -> Result<u64, String> {
    if cap_mb < MIN_CAP_MB {
        return Err(format!(
            "memory limit of {cap_mb} MB is below the minimum of {MIN_CAP_MB} MB (bazel-rs's own startup footprint exceeds it); raise the limit or unset it for {DEFAULT_POLICY}"
        ));
    }
    Ok(cap_mb)
}

/// Total machine memory in MB from /proc/meminfo (MemTotal). None off
/// Linux or when unparsable.
pub fn mem_total_mb() -> Option<u64> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in meminfo.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: u64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
            return Some(kb / 1024);
        }
    }
    None
}

/// The default cap for a host of `total_mb` MemTotal: the smaller of
/// 16 GB and 50% of MemTotal (MEM2; 1 GB = 1024 MB).
///
/// | MemTotal   | default cap        |
/// |------------|--------------------|
/// | >= 32 GB   | 16 GB (16,384 MB)  |
/// | < 32 GB    | 50% of MemTotal    |
///
/// Rationale: the goal for bazel-rs on large monorepos is a peak of about
/// 6-8 GB (warm `--nobuild` of ~/code's largest measured target sets now
/// peaks at ~1.7-3.1 GB, docs/MEMORY.md), so 16 GB is a safety net with
/// generous headroom, not a target: a runaway invocation stops long
/// before it can starve a shared host. The previous sliding scale (70%
/// below 32 GB, 50% to 128 GB, 64 GB flat above) let one invocation take
/// 64 GB on the 251 GB development host. Half of a small host's memory
/// still leaves room for the actions bazel-rs runs (the cap bounds
/// bazel-rs itself, not its children).
pub fn default_cap_for_total_mb(total_mb: u64) -> u64 {
    const GB: u64 = 1024;
    (total_mb / 2).min(16 * GB)
}

/// One-line description of the default for `total_mb`, for the
/// once-per-run announcement (e.g. `16384 MB (the 16 GB default cap;
/// at most 50% of machine memory)`).
pub fn default_cap_description(total_mb: u64, cap_mb: u64) -> String {
    const GB: u64 = 1024;
    if total_mb / 2 >= 16 * GB {
        format!("{cap_mb} MB (the 16 GB default cap; at most 50% of machine memory)")
    } else {
        format!("{cap_mb} MB (50% of machine memory, below the 16 GB default cap)")
    }
}

/// The installed cap when neither flag nor env is set, with its
/// announcement description. None when MemTotal is unknown (no cap,
/// as before) or the computed cap is below the satisfiable minimum.
pub fn default_cap() -> Option<(u64, String)> {
    let total = mem_total_mb()?;
    let cap = default_cap_for_total_mb(total);
    (cap >= MIN_CAP_MB).then(|| (cap, default_cap_description(total, cap)))
}

/// Back-compat shim: the default cap alone, without its description.
pub fn default_cap_mb() -> Option<u64> {
    default_cap().map(|(cap, _)| cap)
}

/// Per-loading-worker heap budget for the auto thread bound (perf
/// follow-up 3): each `--loading_phase_threads` worker evaluates
/// packages on its own Starlark heap; 64 MB per worker is the planning
/// allowance (the 16 MiB stack is virtual-cheap beside it). With the
/// minimum explicit cap (256 MB) this still floors at 2 workers.
pub const WORKER_HEAP_MB: u64 = 64;

/// Bound an AUTO loading-thread count by the memory cap (perf
/// follow-up 3): `min(auto_threads, cap_mb / 64 MB per worker)`,
/// floored at 2 so a tiny-but-legal cap still parallelizes. Only the
/// "auto" expansion is bounded — an explicit
/// `--loading_phase_threads=N` is the user's stated intent and is
/// never rewritten (the watchdog still enforces the cap). `None` cap
/// (MemTotal unknown, no watchdog) leaves auto unbounded.
pub fn bound_auto_loading_threads(auto_threads: usize, cap_mb: Option<u64>) -> usize {
    match cap_mb {
        Some(cap_mb) => {
            let by_memory = (cap_mb / WORKER_HEAP_MB).max(2) as usize;
            auto_threads.min(by_memory).max(1)
        }
        None => auto_threads.max(1),
    }
}

/// Resolve the effective cap: explicit (flag over env, both validated
/// by the caller) wins; otherwise the default cap. The
/// returned bool marks the default so the caller can say so once.
pub fn effective_cap(explicit_mb: Option<u64>) -> Option<(u64, bool)> {
    match explicit_mb {
        Some(cap_mb) => Some((cap_mb, false)),
        None => default_cap_mb().map(|cap_mb| (cap_mb, true)),
    }
}

/// Current RSS in bytes from /proc/self/statm (Linux); None elsewhere.
pub fn rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    let page = page_size_bytes()?;
    Some(resident_pages.saturating_mul(page))
}

fn page_size_bytes() -> Option<u64> {
    // statm counts pages; convert with AT_PAGESZ (6) from /proc/self/auxv
    // (the `param` rustix feature is not enabled). Fallback: 4096.
    let auxv = std::fs::read("/proc/self/auxv").ok()?;
    for chunk in auxv.as_chunks::<16>().0 {
        let key = u64::from_ne_bytes(chunk[..8].try_into().ok()?);
        let val = u64::from_ne_bytes(chunk[8..].try_into().ok()?);
        if key == 6 && val > 0 {
            return Some(val);
        }
        if key == 0 {
            break;
        }
    }
    Some(4096)
}

/// Install the ceiling: the watchdog thread only (no RLIMIT_AS — see
/// the module docs). Idempotent. Call from main::run startup before
/// any heavy work, once the effective cap is resolved.
pub fn install_ceiling(cap_mb: u64) {
    // Debug/test hook: announce installation so a harness sampling
    // /proc/<pid>/limits knows the ceiling is in force (the RLIMIT_AS
    // regression check reads the limits only after this line).
    if std::env::var_os("BAZEL_RS_DEBUG_WATCHDOG").is_some_and(|v| !v.is_empty()) {
        eprintln!("bazel-rs: debug: watchdog installed (cap {cap_mb} MB)");
    }
    start_watchdog(cap_mb);
}

/// Peak RSS (VmHWM) in bytes from /proc/self/status, for reporting.
pub fn peak_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            let kb: u64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
            return Some(kb * 1024);
        }
    }
    None
}

/// Print the final peak line at normal exit when an EXPLICIT cap is
/// installed (BAZEL_RS_TIMING-style diagnostics; stderr, once). The
/// default cap is silent apart from its one-line announcement: a peak
/// line on every run would drown the build summary (and broke the
/// summary-line parsers). No-op without any cap.
pub fn report_peak(cap_mb: Option<u64>) {
    if cap_mb.is_none() || DEFAULT_CAP_ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    if let Some(peak) = peak_rss_bytes() {
        crate::note!(
            "bazel-rs: peak RSS (VmHWM) {} MB (memory limit {} MB)",
            peak / (1024 * 1024),
            cap_mb.unwrap_or(0)
        );
    }
}

fn start_watchdog(cap_mb: u64) {
    if WATCHDOG_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let cap_bytes = cap_mb.saturating_mul(1024 * 1024);
    let warn_at = cap_bytes / 4 * 3;
    std::thread::Builder::new()
        .name("memory-watchdog".to_owned())
        .spawn(move || {
            let mut warned = false;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(250));
                let Some(rss) = rss_bytes() else {
                    continue; // no /proc (non-Linux): no watchdog enforcement
                };
                // The watchdog's lines are diagnostics: never part of what
                // an analysis snapshot captures of planning's stderr
                // (crate::snapshot::diag).
                if rss > cap_bytes {
                    crate::snapshot::diag::line_fmt(format_args!(
                        "bazel-rs: memory limit of {cap_mb} MB exceeded (killed)"
                    ));
                    // Stop new work first, then SIGKILL every live child
                    // group so spawned actions/tests do not outlive the
                    // kill as orphans, then exit with Bazel's OOM code
                    // (after forwarding what a stderr capture still holds).
                    STOP_REQUESTED.store(true, Ordering::SeqCst);
                    kill_live_groups();
                    crate::snapshot::capture::restore_for_exit();
                    crate::progress::abandon();
                    std::process::exit(LIMIT_EXIT_CODE);
                }
                if !warned && rss > warn_at && !crate::ui::quiet() {
                    warned = true;
                    crate::snapshot::diag::line_fmt(format_args!(
                        "bazel-rs: warning: memory usage {} MB exceeds 75% of the {cap_mb} MB limit",
                        rss / (1024 * 1024)
                    ));
                }
            }
        })
        .expect("memory watchdog thread");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_parsing_accepts_positive_integers_only() {
        assert_eq!(parse_cap_mb("6144"), Some(6144));
        assert_eq!(parse_cap_mb(" 8192 "), Some(8192));
        assert_eq!(parse_cap_mb(""), None);
        assert_eq!(parse_cap_mb("0"), None);
        assert_eq!(parse_cap_mb("-1"), None);
        assert_eq!(parse_cap_mb("6GB"), None);
        assert_eq!(parse_cap_mb("1.5"), None);
    }

    #[test]
    fn caps_below_the_minimum_are_rejected_with_a_clear_message() {
        let error = validate_cap_mb(64).unwrap_err();
        assert!(error.contains("below the minimum of 256 MB"), "{error}");
        assert!(error.contains("the default cap"), "{error}");
        assert!(error.contains("smaller of 16 GB and 50%"), "{error}");
        assert!(validate_cap_mb(255).unwrap_err().contains("256 MB"));
        assert_eq!(validate_cap_mb(256), Ok(256));
        assert_eq!(validate_cap_mb(32768), Ok(32768));
    }

    #[test]
    fn default_cap_is_the_smaller_of_16_gb_and_half_the_host() {
        const GB: u64 = 1024;
        // Below 32 GB: half of machine memory.
        assert_eq!(default_cap_for_total_mb(8 * GB), 4 * GB);
        assert_eq!(default_cap_for_total_mb(16 * GB), 8 * GB);
        assert_eq!(default_cap_for_total_mb(32 * GB - 2), 16 * GB - 1);
        // From 32 GB up: 16 GB, however large the host (the 251 GB host
        // that motivated the old 64 GB tier: NOT 64 GB, NOT ~128 GB).
        assert_eq!(default_cap_for_total_mb(32 * GB), 16 * GB);
        assert_eq!(default_cap_for_total_mb(64 * GB), 16 * GB);
        assert_eq!(default_cap_for_total_mb(128 * GB), 16 * GB);
        assert_eq!(default_cap_for_total_mb(251 * GB), 16 * GB);
        assert_eq!(default_cap_for_total_mb(1024 * GB), 16 * GB);
        // Descriptions name the rule that produced the cap.
        assert!(default_cap_description(16 * GB, 8 * GB).contains("50% of machine memory"));
        assert!(default_cap_description(16 * GB, 8 * GB).contains("below the 16 GB"));
        assert!(default_cap_description(251 * GB, 16 * GB).contains("the 16 GB default cap"));
        assert!(default_cap_description(32 * GB, 16 * GB).contains("the 16 GB default cap"));
    }

    #[test]
    fn default_cap_matches_mem_total() {
        if let Some(total) = mem_total_mb() {
            assert!(total > 1024, "this host reports {total} MB");
            let expected = default_cap_for_total_mb(total);
            assert_eq!(default_cap_mb(), Some(expected));
            assert_eq!(effective_cap(None), Some((expected, true)));
            assert_eq!(effective_cap(Some(4096)), Some((4096, false)));
            let (cap, description) = default_cap().expect("default on this host");
            assert_eq!(cap, expected);
            assert!(description.contains(&format!("{cap} MB")), "{description}");
        }
    }

    #[test]
    fn auto_loading_threads_bound_by_cap() {
        // No cap: auto is untouched.
        assert_eq!(bound_auto_loading_threads(64, None), 64);
        // This host's default (16 GB): 16384/64 = 256 » 64 CPUs.
        assert_eq!(bound_auto_loading_threads(64, Some(16 * 1024)), 64);
        // 2048 MB cap: 2048/64 = 32 workers max.
        assert_eq!(bound_auto_loading_threads(64, Some(2048)), 32);
        assert_eq!(bound_auto_loading_threads(8, Some(2048)), 8);
        // Floor of 2 even at the minimum legal cap; never below 1.
        assert_eq!(bound_auto_loading_threads(64, Some(256)), 4);
        assert_eq!(bound_auto_loading_threads(64, Some(64)), 2);
        assert_eq!(bound_auto_loading_threads(1, Some(64)), 1);
    }

    #[test]
    fn oom_exit_code_is_bazels_33() {
        assert_eq!(LIMIT_EXIT_CODE, 33);
    }

    #[test]
    fn rss_and_peak_are_readable_on_linux() {
        if std::path::Path::new("/proc/self/statm").exists() {
            let rss = rss_bytes().expect("statm resident pages");
            assert!(rss > 0);
            let peak = peak_rss_bytes().expect("VmHWM");
            assert!(peak >= rss || peak > 0);
        }
    }

    #[test]
    fn live_group_registry_dedups_and_reports() {
        register_live_group(4_200_001);
        register_live_group(4_200_001);
        register_live_group(4_200_002);
        let groups = live_groups();
        assert_eq!(groups.iter().filter(|g| **g == 4_200_001).count(), 1);
        assert!(groups.contains(&4_200_002));
        // An ended supervised spawn leaves the registry.
        register_live_group(4_200_003);
        unregister_live_group(4_200_003);
        assert!(!live_groups().contains(&4_200_003));
        assert!(live_groups().contains(&4_200_002));
        // Killing a group with no members is ESRCH = success, never an
        // error that could block the watchdog exit.
        assert!(crate::git_process::kill_group(4_200_001).is_ok());
    }
}
