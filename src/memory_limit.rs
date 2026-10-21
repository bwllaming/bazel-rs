//! Hard process memory ceiling (native-only safety valve).
//!
//! `BAZEL_RS_MAX_MEMORY_MB` (environment) or `--memory-limit=<MB>`
//! (build/run/test option; flag wins over env, Bazel's CLI-over-env
//! convention) caps this process:
//!
//! * At startup, before any heavy work, soft+hard `RLIMIT_AS` are set via
//!   rustix to 4x the cap (floor 4 GB: virtual space runs ~3-4x RSS with
//!   many threads/mmaps, measured on //src/... — a 1x or 2x rlimit aborts
//!   the allocator at ~34s while RSS is still ~2 GB), so the ceiling covers the whole
//!   process including the sandbox worker (a re-exec'd child: RLIMIT_AS
//!   is inherited across exec, and its allocations count against the
//!   same inherited limit). On a platform without RLIMIT_AS, or when
//!   setrlimit fails, a startup warning is printed and the process
//!   continues uncapped by rlimit (the watchdog still applies where
//!   /proc exists).
//! * A watchdog thread polls `/proc/self/statm` every 250 ms. RSS over
//!   the cap prints exactly `bazel-rs: memory limit of {cap} MB exceeded
//!   (killed)` to stderr and exits with code 42. Crossing 75% of the cap
//!   warns ONCE. Rust allocation failure itself cannot be mapped to a
//!   graceful error (the allocator aborts via handle_alloc_error), so
//!   the watchdog is the graceful path; RLIMIT_AS is the hard backstop
//!   that turns runaway virtual growth into that abort instead of an
//!   OOM-kill of the whole machine.
//!
//! Caveat: exit(42) kills this process only; already-spawned children
//! (e.g. a test binary) are reparented, not group-killed. The sandbox
//! worker is started per action and checks its own inherited limit, so
//! in practice it dies with its next allocation; a long-running child
//! that never allocates may outlive the parent.

use std::sync::atomic::{AtomicBool, Ordering};

/// Exit code when the watchdog kills the process for exceeding the cap.
pub const LIMIT_EXIT_CODE: i32 = 42;

static WATCHDOG_STARTED: AtomicBool = AtomicBool::new(false);

/// Parse a cap in MB from the environment value. `None` = unset/invalid
/// (invalid values warn at the call site; empty means unset).
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

/// Set soft+hard RLIMIT_AS for `cap_mb`. The user's cap is an RSS
/// budget; RLIMIT_AS counts virtual space (thread stacks, mmaps,
/// reserved arenas), which on this workload runs ~3-4x RSS. The rlimit
/// is therefore set to `cap * AS_HEADROOM`, floored at `AS_FLOOR_MB` so
/// small caps still let the process start; the watchdog enforces the
/// exact RSS cap. Err explains why (caller warns and continues, per
/// the no-RLIMIT_AS platform contract).
pub fn apply_rlimit_as(cap_mb: u64) -> Result<(), String> {
    const AS_HEADROOM: u64 = 4;
    const AS_FLOOR_MB: u64 = 4096;
    let as_mb = cap_mb
        .checked_mul(AS_HEADROOM)
        .map(|mb| mb.max(AS_FLOOR_MB))
        .unwrap_or(u64::MAX);
    let bytes = as_mb
        .checked_mul(1024 * 1024)
        .ok_or_else(|| format!("memory limit of {cap_mb} MB overflows address space"))?;
    rustix::process::setrlimit(
        rustix::process::Resource::As,
        rustix::process::Rlimit {
            current: Some(bytes),
            maximum: Some(bytes),
        },
    )
    .map_err(|e| e.to_string())
}

/// Install the ceiling: rlimit first (warn-and-continue on failure),
/// then the watchdog thread. Idempotent. Call from main::startup
/// before any heavy work, once the effective cap is resolved.
pub fn install_ceiling(cap_mb: u64) {
    if let Err(error) = apply_rlimit_as(cap_mb) {
        eprintln!(
            "bazel-rs: warning: cannot set RLIMIT_AS to {cap_mb} MB ({error}); \
             continuing with watchdog-only memory ceiling"
        );
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

/// Print the final peak line at normal exit when a cap is installed
/// (BAZEL_RS_TIMING-style diagnostics; stderr, once). No-op without cap.
pub fn report_peak(cap_mb: Option<u64>) {
    if cap_mb.is_none() {
        return;
    }
    if let Some(peak) = peak_rss_bytes() {
        eprintln!(
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
                    continue; // no /proc (non-Linux): rlimit is the only cap
                };
                if rss > cap_bytes {
                    eprintln!("bazel-rs: memory limit of {cap_mb} MB exceeded (killed)");
                    std::process::exit(LIMIT_EXIT_CODE);
                }
                if !warned && rss > warn_at {
                    warned = true;
                    eprintln!(
                        "bazel-rs: warning: memory usage {} MB exceeds 75% of the {cap_mb} MB limit",
                        rss / (1024 * 1024)
                    );
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
    fn rss_and_peak_are_readable_on_linux() {
        if std::path::Path::new("/proc/self/statm").exists() {
            let rss = rss_bytes().expect("statm resident pages");
            assert!(rss > 0);
            let peak = peak_rss_bytes().expect("VmHWM");
            assert!(peak >= rss || peak > 0);
        }
    }

    #[test]
    fn rlimit_round_trip_in_child_process() {
        // Setting RLIMIT_AS in-process would cap the test runner itself;
        // verify in a short-lived child via the same rustix calls.
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(exe)
            .args([
                "--exact",
                "memory_limit::tests::rlimit_child_body",
                "--ignored",
                "--nocapture",
            ])
            .env("BAZEL_RS_MEMORY_LIMIT_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    #[ignore = "child body for rlimit_round_trip_in_child_process"]
    fn rlimit_child_body() {
        if std::env::var_os("BAZEL_RS_MEMORY_LIMIT_CHILD").is_none() {
            return;
        }
        apply_rlimit_as(512).unwrap();
        let limit = rustix::process::getrlimit(rustix::process::Resource::As);
        // 4x headroom over the RSS cap, floored at 4096 MB (see apply_rlimit_as).
        assert_eq!(limit.current, Some(4096 * 1024 * 1024));
    }
}
