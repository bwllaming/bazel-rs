//! The execution statistics of Bazel's build summary
//! (runtime/BuildSummaryStatsModule.java, SpawnStats.java): `INFO: N
//! processes: …` and the `Critical Path: X.XXs` of `INFO: Elapsed time: …`.
//!
//! * N counts the actions that completed (ActionCompletionEvent): executed
//!   actions, also those that ran no process (`internal`, e.g. a file
//!   write or the workspace status action, executed on every build), but
//!   not the action cache hits, which are listed apart (`action cache
//!   hit`, the native action cache's hits).
//! * Then every spawn by its runner name (SpawnResult.getRunnerName):
//!   `remote cache hit` first, the others alphabetically.
//! * The critical path is the longest chain of action execution times
//!   through the action graph (crate::scheduler sets
//!   crate::action::ActionResult::critical); test actions are not on it.
//!
//! The same numbers are Bazel's `N total actions` of `INFO: Build
//! completed …` (UiStateTracker counts the completed actions too).

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Default)]
struct Stats {
    /// Completed (non-cache-hit) actions.
    actions: usize,
    /// Completed actions that ran at least one spawn.
    with_spawns: usize,
    action_cache_hits: usize,
    runners: BTreeMap<&'static str, usize>,
    critical: Duration,
}

static STATS: Mutex<Stats> = Mutex::new(Stats {
    actions: 0,
    with_spawns: 0,
    action_cache_hits: 0,
    runners: BTreeMap::new(),
    critical: Duration::ZERO,
});

fn with<T>(f: impl FnOnce(&mut Stats) -> T) -> T {
    f(&mut STATS.lock().unwrap_or_else(|e| e.into_inner()))
}

/// The runners reported before `internal` (SpawnStats.REPORT_FIRST).
const REPORT_FIRST: &[&str] = &["disk cache hit", "remote cache hit"];

/// An action completed; `spawns` are the runner names of its spawns
/// (empty: an internal action).
pub(crate) fn action_completed(spawns: &[&'static str]) {
    with(|stats| {
        stats.actions += 1;
        if !spawns.is_empty() {
            stats.with_spawns += 1;
        }
        for runner in spawns {
            *stats.runners.entry(runner).or_default() += 1;
        }
    });
}

/// An action was an action cache hit.
pub(crate) fn action_cache_hit() {
    with(|stats| stats.action_cache_hits += 1);
}

/// One more spawn of an action counted with [`action_completed`] (a test
/// action's attempts and test.xml generation).
pub(crate) fn spawn(runner: &'static str) {
    with(|stats| *stats.runners.entry(runner).or_default() += 1);
}

/// A test action ran its spawns (counted with [`spawn`]).
pub(crate) fn test_completed() {
    with(|stats| {
        stats.actions += 1;
        stats.with_spawns += 1;
    });
}

/// The critical path is at least `length`.
pub(crate) fn critical_path_at_least(length: Duration) {
    with(|stats| stats.critical = stats.critical.max(length));
}

/// The completed actions (`N total actions`).
pub fn completed_actions() -> usize {
    with(|stats| stats.actions)
}

/// The critical path's length (`Critical Path: X.XXs`,
/// AggregatedCriticalPath.toStringSummaryNoRemote).
pub fn critical_path() -> Duration {
    with(|stats| stats.critical)
}

/// `N processes: …` (SpawnStats.convertSummaryToString), without `INFO: `.
pub fn processes_text() -> String {
    with(|stats| {
        let total = stats.actions;
        if total == 0 {
            return "0 processes.".to_owned();
        }
        let mut parts: Vec<(usize, &str)> = Vec::new();
        if stats.action_cache_hits > 0 {
            parts.push((stats.action_cache_hits, "action cache hit"));
        }
        for runner in REPORT_FIRST {
            if let Some(&count) = stats.runners.get(runner)
                && count > 0
            {
                parts.push((count, runner));
            }
        }
        if stats.with_spawns < total {
            parts.push((total - stats.with_spawns, "internal"));
        }
        for (runner, &count) in &stats.runners {
            if !REPORT_FIRST.contains(runner) && count > 0 {
                parts.push((count, runner));
            }
        }
        let mut text = format!("{total} process{}", if total == 1 { "" } else { "es" });
        for (index, (count, name)) in parts.iter().enumerate() {
            text.push_str(if index == 0 { ": " } else { ", " });
            text.push_str(&format!("{count} {name}"));
        }
        text.push('.');
        text
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(stats: Stats) -> String {
        *STATS.lock().unwrap() = stats;
        processes_text()
    }

    /// Genuine captures (tests/fixtures/console_text, tests/fixtures/build_results).
    #[test]
    fn formats_like_spawn_stats() {
        assert_eq!(text(Stats::default()), "0 processes.");
        let mut stats = Stats {
            actions: 1,
            ..Stats::default()
        };
        assert_eq!(text(stats), "1 process: 1 internal.");
        stats = Stats {
            actions: 2,
            with_spawns: 1,
            action_cache_hits: 1,
            ..Stats::default()
        };
        stats.runners.insert("processwrapper-sandbox", 1);
        assert_eq!(
            text(stats),
            "2 processes: 1 action cache hit, 1 internal, 1 processwrapper-sandbox."
        );
        stats = Stats {
            actions: 3,
            with_spawns: 3,
            ..Stats::default()
        };
        stats.runners.insert("linux-sandbox", 1);
        stats.runners.insert("remote cache hit", 1);
        stats.runners.insert("local", 1);
        assert_eq!(
            text(stats),
            "3 processes: 1 remote cache hit, 1 linux-sandbox, 1 local."
        );
        stats = Stats {
            actions: 1,
            with_spawns: 1,
            ..Stats::default()
        };
        stats.runners.insert("local", 2);
        assert_eq!(text(stats), "1 process: 2 local.");
        *STATS.lock().unwrap() = Stats::default();
    }
}
