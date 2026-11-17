//! Bazel's progress messages without cursor control (docs/PROGRESS.md
//! "Without a terminal"): when the curses display is off — stderr is not a
//! terminal, `--curses=no`, `CI` — Bazel still prints its progress bar,
//! one plain line at a time (UiEventHandler with `cursorControl` false:
//! `writeProgressBar(…, shortVersion = true)`, never cleared), at most once
//! per `max(--show_progress_rate_limit, 1s)` (NO_CURSES_MINIMAL_PROGRESS
//! _RATE_LIMIT) and only when something happened (no time-based refresh).
//!
//! ```text
//! Computing main repo mapping:
//! Loading:
//! Loading: 0 packages loaded
//! Analyzing: target //:all (5 packages loaded, 6 targets configured)
//! Analyzing: target //:all (5 packages loaded, 6 targets configured)
//!
//! INFO: Analyzed target //:all (27 packages loaded, 33 targets configured).
//! [22 / 26] Executing genrule //:slow0; 1s processwrapper-sandbox ... (3 actions running)
//! [24 / 26] Executing genrule //:slow2; 3s processwrapper-sandbox ... (2 actions running)
//! ```
//!
//! The phase starts print at once (ignoreRefreshLimitOnce): `Computing main
//! repo mapping: ` when the command starts, `Loading: ` and `Loading: N
//! packages loaded` when target patterns are evaluated, `Analyzing: …` when
//! analysis starts; the next analysis line once more without the limit and
//! followed by an empty line (Skymeld's ANALYSIS_AND_EXECUTION state, after
//! the execution progress receiver is announced — at the latest just before
//! `INFO: Analyzed …`). Later lines come from [`run`]'s thread when the
//! sampled state changed (an event in Bazel), a fetch is in flight (its
//! progress events), or the inactivity watchdog fires (ActionExecution
//! InactivityWatchdog: no action completed for 11 s, then 30 s more, then
//! every 60 s). Every line goes through the diagnostics stream
//! (crate::snapshot::diag): never part of an analysis snapshot's window.

use super::render::{self, Active, View};
use super::{COUNTERS, Phase};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// The thread's sampling period (how soon a due line follows its event).
const TICK: Duration = Duration::from_millis(50);
/// UiEventHandler.NO_CURSES_MINIMAL_PROGRESS_RATE_LIMIT.
const MINIMAL_RATE: Duration = Duration::from_secs(1);
/// UiStateTracker.SHOW_TIME_THRESHOLD_SECONDS.
const SHOW_TIME_THRESHOLD_SECONDS: u64 = 3;

/// Whether the plain lines are on (with the hooks, [`super::enabled`]).
static PLAIN: AtomicBool = AtomicBool::new(false);

pub(super) fn on() -> bool {
    PLAIN.load(Ordering::Relaxed)
}

/// The progress rate limit without cursor control: `--show_progress_rate
/// _limit` in milliseconds (rounded), at least one second.
pub(super) fn rate(seconds: f64) -> Duration {
    let millis = if seconds.is_finite() {
        (seconds * 1000.0).round().clamp(0.0, 1e12) as u64
    } else {
        0
    };
    Duration::from_millis(millis).max(MINIMAL_RATE)
}

/// The emitter's state: what was last printed and when.
struct Emitter {
    rate: Duration,
    color: bool,
    last: Option<Instant>,
    /// The next line ignores the rate limit (ignoreRefreshLimitOnce).
    force: bool,
    /// The execution progress receiver is announced: an analysis line is
    /// followed by an empty one.
    receiver: bool,
    /// The analysis reported its summary: no `Analyzing:` line after it.
    analyzed: bool,
    /// The state the last tick saw.
    signature: Option<Signature>,
    /// The update thread's next wake (every rate interval).
    next_wake: Instant,
    /// Something happened since the last line (progressBarNeedsRefresh).
    pending: bool,
    /// The inactivity watchdog: when it fires next, the interval after it.
    watchdog: Option<(Instant, Duration)>,
    watched_done: usize,
}

static EMITTER: Mutex<Option<Emitter>> = Mutex::new(None);

fn emitter() -> std::sync::MutexGuard<'static, Option<Emitter>> {
    EMITTER.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Stops the thread ([`stop`]).
struct Stop {
    stopped: Mutex<bool>,
    signal: Condvar,
}

static STOP: Stop = Stop {
    stopped: Mutex::new(false),
    signal: Condvar::new(),
};
static THREAD: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

/// What makes a line due: the state an event would change.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Signature {
    phase: Phase,
    packages: usize,
    configured: usize,
    aspects: usize,
    actions_total: usize,
    actions_done: usize,
    tests_done: usize,
    active: Vec<(u64, bool, Option<super::Strategy>)>,
}

impl Signature {
    fn of(view: &View, aspects: usize) -> Self {
        Self {
            phase: view.phase,
            packages: view.packages,
            configured: view.configured,
            aspects,
            actions_total: view.actions_total,
            actions_done: view.actions_done,
            tests_done: view.tests_done,
            active: view
                .active
                .iter()
                .map(|active| (active.started_ms, active.test, active.strategy))
                .collect(),
        }
    }
}

/// Start the plain lines: the hooks count, the thread samples, and the
/// first line (`Computing main repo mapping: `) prints now.
pub(super) fn start(rate: Duration, color: bool) -> std::io::Result<()> {
    {
        let mut slot = emitter();
        if slot.is_some() {
            return Ok(());
        }
        *slot = Some(Emitter {
            rate,
            color,
            last: None,
            force: true,
            receiver: false,
            analyzed: false,
            signature: None,
            next_wake: Instant::now() + rate,
            pending: false,
            watchdog: None,
            watched_done: 0,
        });
    }
    *STOP.stopped.lock().unwrap_or_else(PoisonError::into_inner) = false;
    PLAIN.store(true, Ordering::Relaxed);
    super::ACTIVE.store(true, Ordering::Relaxed);
    let handle = std::thread::Builder::new()
        .name("progress-lines".to_owned())
        .spawn(run);
    match handle {
        Ok(handle) => *THREAD.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle),
        Err(error) => {
            stop();
            return Err(error);
        }
    }
    phase_started(None);
    Ok(())
}

/// Stop the thread (before the build summary, or `run`'s target): no line
/// after this. A no-op when the lines are off.
pub(super) fn stop() {
    if !on() {
        return;
    }
    PLAIN.store(false, Ordering::Relaxed);
    super::ACTIVE.store(false, Ordering::Relaxed);
    *STOP.stopped.lock().unwrap_or_else(PoisonError::into_inner) = true;
    STOP.signal.notify_all();
    let handle = THREAD.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some(handle) = handle
        && handle.thread().id() != std::thread::current().id()
    {
        let _ = handle.join();
    }
    *emitter() = None;
}

/// A phase started (crate::progress::mark): its line prints now, whatever
/// the rate limit. `Loading` prints Bazel's two (BuildStartingEvent, then
/// LoadingPhaseStartedEvent).
pub(super) fn phase_started(phase: Option<Phase>) {
    let mut slot = emitter();
    let Some(emitter) = slot.as_mut() else {
        return;
    };
    let view = super::sample();
    let aspects = COUNTERS.aspects.load(Ordering::Relaxed);
    let mut lines = Vec::new();
    match phase {
        None => lines.push(base("Computing main repo mapping", "", emitter.color)),
        Some(Phase::Loading) => {
            lines.push(base("Loading", "", emitter.color));
            lines.push(base(
                "Loading",
                &format!("{} packages loaded", view.packages),
                emitter.color,
            ));
        }
        Some(Phase::Analyzing) => {
            lines.push(analyzing(&view, aspects, emitter.color));
        }
        _ => return,
    }
    let now = Instant::now();
    emitter.write(&lines, now);
    emitter.signature = Some(Signature::of(&view, aspects));
    if phase == Some(Phase::Analyzing) {
        // ExecutionProgressReceiverAvailableEvent: ignoreRefreshLimitOnce,
        // and the analysis lines are Skymeld's ANALYSIS_AND_EXECUTION ones.
        emitter.force = true;
        emitter.receiver = true;
    }
}

/// The analysis is about to report `INFO: Analyzed …`: the analysis line
/// still due without the rate limit prints first; none after it.
pub(super) fn analysis_reported() {
    let mut slot = emitter();
    let Some(emitter) = slot.as_mut() else {
        return;
    };
    if emitter.analyzed {
        return;
    }
    if emitter.force && emitter.receiver {
        let view = super::sample();
        let aspects = COUNTERS.aspects.load(Ordering::Relaxed);
        if view.phase == Phase::Analyzing {
            let lines = [analyzing(&view, aspects, emitter.color), String::new()];
            emitter.write(&lines, Instant::now());
        }
    }
    emitter.force = false;
    emitter.analyzed = true;
}

impl Emitter {
    fn write(&mut self, lines: &[String], now: Instant) {
        let mut bytes = Vec::new();
        for line in lines {
            bytes.extend_from_slice(line.as_bytes());
            bytes.push(b'\n');
        }
        // What the process printed into an analysis snapshot's capture
        // before this line goes out before it.
        crate::snapshot::capture::settle(Duration::from_millis(100));
        crate::snapshot::diag::write(&bytes);
        self.last = Some(now);
        self.force = false;
        self.pending = false;
    }

    /// One tick of the thread: print the progress line if one is due.
    fn tick(&mut self, now: Instant) {
        let view = super::sample();
        let aspects = COUNTERS.aspects.load(Ordering::Relaxed);
        let signature = Signature::of(&view, aspects);
        // Bazel refreshes at once (if the rate allows) on the events that
        // start something — a phase, a package, a configured target, an
        // action starting or starting its spawn — and leaves a completion
        // (refreshSoon) to its update thread's next wake.
        let (started, completed) = match &self.signature {
            None => (true, false),
            Some(previous) => (
                previous.phase != signature.phase
                    || previous.packages != signature.packages
                    || previous.configured != signature.configured
                    || previous.aspects != signature.aspects
                    || previous.actions_total != signature.actions_total
                    || signature
                        .active
                        .iter()
                        .any(|active| !previous.active.contains(active)),
                previous != &signature,
            ),
        };
        self.signature = Some(signature);
        let executing = view.phase == Phase::Executing;
        // The inactivity watchdog (ActionExecutionInactivityWatchdog):
        // armed while actions are in flight, reset by every completion.
        let mut inactive = false;
        if executing && !view.active.is_empty() {
            match self.watchdog {
                Some((at, next)) if self.watched_done == view.actions_done => {
                    if now >= at {
                        inactive = true;
                        let following = if next.as_secs() <= 10 {
                            Duration::from_secs(30)
                        } else {
                            Duration::from_secs(60)
                        };
                        self.watchdog = Some((now + next, following));
                    }
                }
                _ => {
                    // The watchdog's second after a completion, then its
                    // first ten-second wait.
                    self.watchdog = Some((now + Duration::from_secs(11), Duration::from_secs(30)));
                    self.watched_done = view.actions_done;
                }
            }
        } else {
            self.watchdog = None;
        }
        // A fetch in flight reports progress events; the watchdog's
        // reminder is a progress event too.
        let direct = started || inactive || !view.fetches.is_empty();
        self.pending |= direct || completed;
        // The update thread wakes every rate interval from the start.
        let wake = now >= self.next_wake;
        while self.next_wake <= now {
            self.next_wake += self.rate;
        }
        if !self.pending || !(direct || wake || self.force) {
            return;
        }
        // At most one line per rate interval (a tick's slack: the thread's
        // consecutive wakes are one interval apart).
        let allowed = self.force || self.last.is_none_or(|last| now + TICK >= last + self.rate);
        if !allowed {
            return;
        }
        let lines = match view.phase {
            Phase::Starting => vec![base("Computing main repo mapping", "", self.color)],
            Phase::Loading => vec![base(
                "Loading",
                &format!("{} packages loaded", view.packages),
                self.color,
            )],
            Phase::Analyzing if self.analyzed => return,
            Phase::Analyzing => {
                let mut lines = vec![analyzing(&view, aspects, self.color)];
                if self.receiver {
                    lines.push(String::new());
                }
                lines
            }
            Phase::SnapshotHit => return,
            Phase::Executing => match execution(&view, self.color) {
                Some(line) => vec![line],
                None => return,
            },
        };
        self.write(&lines, now);
    }
}

/// The thread: a tick every [`TICK`] until [`stop`].
fn run() {
    let mut stopped = STOP.stopped.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        let (guard, _) = STOP
            .signal
            .wait_timeout_while(stopped, TICK, |stopped| !*stopped)
            .unwrap_or_else(PoisonError::into_inner);
        stopped = guard;
        if *stopped {
            return;
        }
        drop(stopped);
        if let Some(emitter) = emitter().as_mut() {
            emitter.tick(Instant::now());
        }
        stopped = STOP.stopped.lock().unwrap_or_else(PoisonError::into_inner);
    }
}

fn green(text: &str, color: bool) -> String {
    if color {
        format!("{}{text}{}", render::GREEN, render::RESET)
    } else {
        text.to_owned()
    }
}

/// SkymeldUiStateTracker.writeBaseProgress: `STATUS: MESSAGE`.
fn base(status: &str, message: &str, color: bool) -> String {
    format!("{} {message}", green(&format!("{status}:"), color))
}

/// `Analyzing: target //x (P packages loaded, T targets configured)`
/// (writeLoadingAnalysisPhaseProgress; AnalysisProgressReceiver).
fn analyzing(view: &View, aspects: usize, color: bool) -> String {
    let what = super::analysis_subject(view.targets);
    let mut progress = format!(
        "{} packages loaded, {} configured",
        view.packages,
        plural(view.configured, "target")
    );
    if aspects > 0 {
        progress.push_str(&format!(", {}", plural(aspects, "aspect application")));
    }
    format!("{} ({progress})", base("Analyzing", &what, color))
}

fn plural(count: usize, word: &str) -> String {
    if count == 1 {
        format!("{count} {word}")
    } else {
        format!("{count} {word}s")
    }
}

/// UiStateTracker.writeExecutionProgress with `shortVersion`: `[done /
/// total]`, the tests once one completed, then the oldest action (running
/// ones first) and, with several, `... (N actions running)`. None without
/// an action in flight (the build is between its actions or done).
fn execution(view: &View, color: bool) -> Option<String> {
    let oldest = view
        .active
        .iter()
        .filter(|active| active.strategy.is_some())
        .min_by_key(|active| active.started_ms)
        .or_else(|| view.active.iter().min_by_key(|active| active.started_ms))?;
    // Bazel's progress receiver counts the test actions too.
    let mut line = green(
        &format!(
            "[{} / {}]",
            render::grouped(view.actions_done + view.tests_done),
            render::grouped(view.actions_total + view.tests_total)
        ),
        color,
    );
    if view.tests_done > 0 {
        line.push_str(&format!(
            " {} / {} tests",
            view.tests_done, view.tests_total
        ));
        if view.tests_failed > 0 {
            let failed = format!("{} failed", view.tests_failed);
            line.push_str(", ");
            if color {
                line.push_str(&format!("{}{failed}{}", render::RED, render::RESET));
            } else {
                line.push_str(&failed);
            }
        }
        line.push(';');
    }
    line.push(' ');
    line.push_str(&describe(view, oldest));
    if view.active.len() > 1 {
        line.push_str(&format!(" ... ({})", count_actions(&view.active)));
    }
    Some(line)
}

/// UiStateTracker.describeAction: `[Prepa] ` before a spawn started (an
/// action checking the cache or staging its inputs, a test before its
/// attempt), the time with the strategy (or past three seconds), no width
/// limit.
fn describe(view: &View, active: &Active) -> String {
    let elapsed = view.now_ms.saturating_sub(active.started_ms) / 1000;
    match active.strategy {
        Some(strategy) => format!("{}; {elapsed}s {}", active.description, strategy.name()),
        None => {
            let postfix = if elapsed > SHOW_TIME_THRESHOLD_SECONDS {
                format!("; {elapsed}s")
            } else {
                String::new()
            };
            format!("[Prepa] {}{postfix}", active.description)
        }
    }
}

/// UiStateTracker.countActions.
fn count_actions(active: &[Active]) -> String {
    let running = active
        .iter()
        .filter(|active| active.strategy.is_some())
        .count();
    if active.len() == 1 {
        " 1 action running".to_owned()
    } else if active.len() == running {
        format!("{} actions running", active.len())
    } else {
        format!("{} actions, {running} running", active.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::Strategy;

    fn view(phase: Phase) -> View {
        View {
            phase,
            now_ms: 10_000,
            targets: 1,
            packages: 5,
            planned: None,
            configured: 6,
            actions_total: 1234,
            actions_done: 22,
            tests_total: 0,
            tests_done: 0,
            tests_failed: 0,
            active: Vec::new(),
            fetches: Vec::new(),
        }
    }

    fn action(description: &str, started_ms: u64, strategy: Option<Strategy>) -> Active {
        Active {
            description: description.to_owned(),
            started_ms,
            test: false,
            strategy,
        }
    }

    #[test]
    fn the_rate_is_at_least_a_second() {
        assert_eq!(rate(0.2), Duration::from_secs(1));
        assert_eq!(rate(-1.0), Duration::from_secs(1));
        assert_eq!(rate(f64::NAN), Duration::from_secs(1));
        assert_eq!(rate(2.5), Duration::from_millis(2500));
    }

    #[test]
    fn lines_follow_the_short_progress_bar() {
        assert_eq!(
            base("Computing main repo mapping", "", false),
            "Computing main repo mapping: "
        );
        assert_eq!(base("Loading", "", false), "Loading: ");
        assert_eq!(
            base("Loading", "0 packages loaded", true),
            "\x1b[32mLoading:\x1b[0m 0 packages loaded"
        );
        let mut two = view(Phase::Analyzing);
        two.targets = 2;
        assert_eq!(
            analyzing(&two, 0, false),
            "Analyzing: 2 targets (5 packages loaded, 6 targets configured)"
        );
        let mut one = view(Phase::Analyzing);
        one.configured = 1;
        one.targets = 3;
        assert_eq!(
            analyzing(&one, 2, false),
            "Analyzing: 3 targets (5 packages loaded, 1 target configured, 2 aspect applications)"
        );

        let mut idle = view(Phase::Executing);
        assert_eq!(execution(&idle, false), None);
        idle.active.push(action(
            "BazelWorkspaceStatusAction stable-status.txt",
            9_000,
            None,
        ));
        assert_eq!(
            execution(&idle, false).unwrap(),
            "[22 / 1,234] [Prepa] BazelWorkspaceStatusAction stable-status.txt"
        );
        let mut many = view(Phase::Executing);
        many.active.push(action(
            "Executing genrule //:slow1",
            9_000,
            Some(Strategy::ProcessWrapperSandbox),
        ));
        many.active.push(action(
            "Executing genrule //:slow0",
            8_500,
            Some(Strategy::ProcessWrapperSandbox),
        ));
        many.active
            .push(action("Executing genrule //:chain", 1_000, None));
        assert_eq!(
            execution(&many, false).unwrap(),
            "[22 / 1,234] Executing genrule //:slow0; 1s processwrapper-sandbox ... (3 actions, 2 running)"
        );
        many.active.pop();
        assert_eq!(
            execution(&many, false).unwrap(),
            "[22 / 1,234] Executing genrule //:slow0; 1s processwrapper-sandbox ... (2 actions running)"
        );
        let mut long = view(Phase::Executing);
        long.active.push(action("Compiling a.cc", 5_000, None));
        assert_eq!(
            execution(&long, false).unwrap(),
            "[22 / 1,234] [Prepa] Compiling a.cc; 5s"
        );
        let mut tests = view(Phase::Executing);
        tests.tests_total = 3;
        tests.tests_done = 2;
        tests.tests_failed = 1;
        tests.active.push(Active {
            description: "Testing //:t2".to_owned(),
            started_ms: 7_000,
            test: true,
            strategy: Some(Strategy::ProcessWrapperSandbox),
        });
        assert_eq!(
            execution(&tests, false).unwrap(),
            "[24 / 1,237] 2 / 3 tests, 1 failed; Testing //:t2; 3s processwrapper-sandbox"
        );
        tests.tests_done = 0;
        tests.tests_failed = 0;
        assert_eq!(
            execution(&tests, false).unwrap(),
            "[22 / 1,237] Testing //:t2; 3s processwrapper-sandbox"
        );
    }
}
