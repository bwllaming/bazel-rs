//! The interactive progress display (docs/PROGRESS.md): Bazel's curses-mode
//! status area (runtime/UiEventHandler.java, UiStateTracker.java) for
//! `build`, `test` and `run` on a terminal.
//!
//! Off — the default whenever stderr and stdout are not both a terminal
//! (Bazel's client `--isatty`), `TERM` is dumb, `CI` is set, or with
//! `--curses=no`, `--noshow_progress` or `BAZEL_RS_PROGRESS=0` — nothing
//! here runs: every hook below is one relaxed atomic load, and the output is
//! byte for byte what it is without this module.
//!
//! On ([`start`]), file descriptor 2 — and 1 when stdout is the same
//! terminal — is redirected into a pipe whose reader thread
//! ([`terminal`]) owns the terminal: it forwards what the process (and any
//! child that inherited the descriptor) writes, whole lines at a time, and
//! keeps a status area of a few lines below it, erased before forwarded
//! output and redrawn after it, refreshed at most every
//! `--show_progress_rate_limit` seconds (0.2 by default, never below 0.1).
//! Every printer is covered by construction, exactly like the analysis
//! snapshot's capture of planning's stderr (crate::snapshot::capture), which
//! nests inside: the capture's "original stderr" is this pipe, and the
//! status area is written to the saved terminal descriptor, never to file
//! descriptor 2 — it can never become part of a snapshot window.
//!
//! The engine reports progress through counters it updates with relaxed
//! atomic stores (packages loaded, configured targets, actions done) and a
//! fixed table of running-action slots written under a sequence lock
//! ([`Running`]): no lock and no allocation per action. Repository fetches
//! and downloads, which are rare, register in a mutexed list ([`Fetching`]).
//! The reader thread samples all of it when it redraws.
//!
//! [`finish`] (every exit path of the CLI) restores the descriptors, lets the
//! thread forward what is still in the pipe, erases the status area and
//! prints the interactive-only summary (`INFO: Elapsed time: …`,
//! `INFO: Build completed successfully, N total actions`); a SIGINT, SIGTERM
//! or SIGHUP erases the area before the signal ends the process.

mod render;
mod terminal;

pub use terminal::{abandon, finish, start};

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Instant;

static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether the progress display is on (every hook's first check).
#[inline]
pub fn enabled() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// The process start, the origin of every timestamp here ([`process_start`]).
static START: OnceLock<Instant> = OnceLock::new();

/// Record the process start (the first thing `main` does); the elapsed time
/// of the summary counts from here.
pub fn process_start() {
    START.get_or_init(Instant::now);
}

fn start_instant() -> Instant {
    *START.get_or_init(Instant::now)
}

/// Milliseconds since [`process_start`] (never 0: 0 marks "unset").
fn now_ms() -> u64 {
    (start_instant().elapsed().as_millis() as u64).max(1)
}

/// The phase the status area describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum Phase {
    /// Module resolution, extensions, the snapshot check (Bazel's
    /// "Computing main repo mapping:").
    Starting = 0,
    /// Target pattern expansion (`Loading: N packages loaded`).
    Loading = 1,
    /// Analysis (`Analyzing: N targets (P packages loaded, T targets
    /// configured)`).
    Analyzing = 2,
    /// An analysis snapshot hit: loading and analysis are skipped.
    SnapshotHit = 3,
    /// The execution phase (`[done / total] …`).
    Executing = 4,
}

impl Phase {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Loading,
            2 => Self::Analyzing,
            3 => Self::SnapshotHit,
            4 => Self::Executing,
            _ => Self::Starting,
        }
    }
}

/// The counters the engine updates (relaxed: the display only samples them).
struct Counters {
    phase: AtomicU8,
    /// Phase 1 of analysis ended: `planned` is the final number of targets
    /// whose implementations phase 2 runs.
    phase1_done: AtomicBool,
    targets: AtomicUsize,
    packages: AtomicUsize,
    planned: AtomicUsize,
    configured: AtomicUsize,
    /// Aspect applications configured (`, N aspect applications`).
    aspects: AtomicUsize,
    actions_total: AtomicUsize,
    actions_done: AtomicUsize,
    tests_total: AtomicUsize,
    tests_done: AtomicUsize,
    tests_failed: AtomicUsize,
}

static COUNTERS: Counters = Counters {
    phase: AtomicU8::new(0),
    phase1_done: AtomicBool::new(false),
    targets: AtomicUsize::new(0),
    packages: AtomicUsize::new(0),
    planned: AtomicUsize::new(0),
    configured: AtomicUsize::new(0),
    aspects: AtomicUsize::new(0),
    actions_total: AtomicUsize::new(0),
    actions_done: AtomicUsize::new(0),
    tests_total: AtomicUsize::new(0),
    tests_done: AtomicUsize::new(0),
    tests_failed: AtomicUsize::new(0),
};

/// Advance the phase (never backwards: a verify-mode shadow analysis after a
/// hit keeps showing execution once it started).
fn advance(phase: Phase) {
    COUNTERS.phase.fetch_max(phase as u8, Ordering::Relaxed);
}

/// A crate::perf_mark: the phase transitions, and the interactive-only
/// `INFO:` line of an analysis snapshot hit.
pub(crate) fn mark(name: &str) {
    if name == "snapshot.hit" {
        // The hit replays the `INFO: Analyzed …` lines of the analysis it
        // saved; the build summary follows at the end like after one.
        SUMMARY.store(true, Ordering::Relaxed);
    }
    if !enabled() {
        return;
    }
    match name {
        "select.start" => advance(Phase::Loading),
        "plan.start" => advance(Phase::Analyzing),
        "plan.phase1_analysis_done" => COUNTERS.phase1_done.store(true, Ordering::Relaxed),
        "snapshot.hit" if COUNTERS.phase.load(Ordering::Relaxed) < Phase::SnapshotHit as u8 => {
            advance(Phase::SnapshotHit);
            info_line("Analysis snapshot hit: loading and analysis skipped.");
        }
        _ => {}
    }
}

/// Whether the command prints Bazel's build summary at its end
/// ([`terminal::finish`]): armed once a `build`, `test` or `run` analysis
/// reported its targets ([`analysis_report`]) or a snapshot hit replayed
/// that report; disarmed when printed (once per command).
static SUMMARY: AtomicBool = AtomicBool::new(false);

/// The end of a successful `build`/`test`/`run` analysis (also with
/// `--nobuild`, `fetch TARGET…`): Bazel's `INFO: Analyzed target //x (P
/// packages loaded, T targets configured).` (UiStateTracker
/// .analysisComplete) and `INFO: Found N targets...` (AnalysisPhaseRunner
/// .reportTargets, or reportTargetsWithTests with the `tests` of `test`),
/// printed in every UI mode. Notes written while planning's stderr is
/// captured: an analysis snapshot hit replays them as they were.
pub(crate) fn analysis_report(labels: &[crate::label::Label], tests: Option<usize>) {
    for line in analysis_lines(
        labels,
        tests,
        COUNTERS.packages.load(Ordering::Relaxed),
        COUNTERS.configured.load(Ordering::Relaxed),
        COUNTERS.aspects.load(Ordering::Relaxed),
    ) {
        crate::ui::note(&line);
    }
    SUMMARY.store(true, Ordering::Relaxed);
}

/// The notes [`analysis_report`] prints for `labels` with these counts
/// (an incremental analysis whose requested labels changed prints them
/// in place of the saved ones, crate::snapshot::incremental).
pub fn analysis_lines(
    labels: &[crate::label::Label],
    tests: Option<usize>,
    packages: usize,
    configured: usize,
    aspects: usize,
) -> Vec<String> {
    let what = match labels {
        [label] => format!("target {}", crate::test_runner::label_print(label)),
        _ => format!("{} targets", labels.len()),
    };
    let mut lines = Vec::new();
    // No targets: nothing analyzed to report (genuine `build` without
    // targets prints only `Found 0 targets...`).
    if !labels.is_empty() {
        lines.push(format!(
            "INFO: {}",
            render::analyzed(&what, packages, configured, aspects)
        ));
    }
    lines.push(format!("INFO: {}", render::found(labels.len(), tests)));
    lines
}

/// The packages loaded so far (the `P packages loaded` of `INFO:
/// Analyzed …`; a derived hit reports what its target pattern phase
/// loaded, crate::snapshot::derive).
pub fn loaded_packages() -> usize {
    COUNTERS.packages.load(Ordering::Relaxed)
}

/// Arm the build summary as [`analysis_report`] does (an incremental
/// analysis that kept the saved plan prints the saved report instead,
/// crate::snapshot::incremental).
pub fn arm_summary() {
    SUMMARY.store(true, Ordering::Relaxed);
}

/// Whether the build summary is due (and now taken: printed once).
fn take_summary() -> bool {
    SUMMARY.swap(false, Ordering::Relaxed)
}

/// The build summary without the display (`--curses=no`, not a terminal):
/// Bazel prints `INFO: Elapsed time: …` and `INFO: Build completed …` in
/// every UI mode. Printed for a successful command (exit 0) and one whose
/// tests failed (3); a failure's own `ERROR:` lines end the others.
fn plain_summary(exit_code: i32) {
    if !take_summary() || !matches!(exit_code, 0 | 3) {
        return;
    }
    for line in render::summary(
        start_instant().elapsed(),
        exit_code,
        &render::Totals::current(),
        COUNTERS.tests_failed.load(Ordering::Relaxed),
        false,
    ) {
        crate::ui::note(&line);
    }
}

/// The build summary of a failed build that reports its failure itself
/// (`--keep_going`'s `ERROR: Build did NOT complete successfully`): Bazel's
/// `INFO: Elapsed time: …` and `INFO: N processes: …` precede that line
/// (BuildSummaryStatsModule.buildComplete). Without the display only (the
/// display prints its summary when it ends).
pub fn failed_build_summary() {
    if enabled() || !take_summary() {
        return;
    }
    for line in render::summary(
        start_instant().elapsed(),
        1,
        &render::Totals::current(),
        0,
        false,
    ) {
        crate::ui::note(&line);
    }
}

/// An interactive-only `INFO: ` line, through the diagnostics stream
/// (crate::snapshot::diag): ordered with what the process printed, never
/// part of an analysis snapshot's window.
fn info_line(text: &str) {
    let color = terminal::color();
    crate::snapshot::diag::line(&render::info(text, color));
}

/// A package was loaded (committed).
#[inline]
pub(crate) fn package_loaded() {
    COUNTERS.packages.fetch_add(1, Ordering::Relaxed);
}

/// The package, aspect-application and configured-target counts
/// (BAZEL_RS_EVAL_VERIFY restores them after its shadow analysis, a
/// speculative phase 2 after its own, so the reported analysis is the
/// planning engine's alone).
pub(crate) fn analysis_counts() -> (usize, usize, usize) {
    (
        COUNTERS.packages.load(Ordering::Relaxed),
        COUNTERS.aspects.load(Ordering::Relaxed),
        COUNTERS.configured.load(Ordering::Relaxed),
    )
}

pub(crate) fn restore_analysis_counts((packages, aspects, configured): (usize, usize, usize)) {
    COUNTERS.packages.store(packages, Ordering::Relaxed);
    COUNTERS.aspects.store(aspects, Ordering::Relaxed);
    COUNTERS.configured.store(configured, Ordering::Relaxed);
}

/// An aspect application was configured (its implementation ran).
#[inline]
pub(crate) fn aspect_applied() {
    COUNTERS.aspects.fetch_add(1, Ordering::Relaxed);
}

/// Analysis of `count` requested targets starts.
#[inline]
pub(crate) fn analysis_targets(count: usize) {
    COUNTERS.targets.store(count, Ordering::Relaxed);
}

/// Phase 1 of analysis has found `count` targets to configure.
#[inline]
pub(crate) fn analysis_planned(count: usize) {
    if enabled() {
        COUNTERS.planned.store(count, Ordering::Relaxed);
    }
}

/// `count` targets are configured (their implementations ran).
#[inline]
pub(crate) fn analysis_configured(count: usize) {
    COUNTERS.configured.store(count, Ordering::Relaxed);
}

/// An execution graph of `actions` actions starts (`test` adds its test
/// actions through [`tests_planned`]).
#[inline]
pub(crate) fn execution_started(actions: usize) {
    COUNTERS.actions_total.fetch_add(actions, Ordering::Relaxed);
    advance(Phase::Executing);
}

/// One action of the execution graph finished (or was skipped).
#[inline]
pub(crate) fn action_done() {
    if enabled() {
        COUNTERS.actions_done.fetch_add(1, Ordering::Relaxed);
    }
}

/// `count` tests will run.
#[inline]
pub(crate) fn tests_planned(count: usize) {
    COUNTERS.tests_total.fetch_add(count, Ordering::Relaxed);
}

/// A test finished (all its runs and shards).
#[inline]
pub(crate) fn test_finished(passed: bool) {
    COUNTERS.tests_done.fetch_add(1, Ordering::Relaxed);
    if !passed {
        COUNTERS.tests_failed.fetch_add(1, Ordering::Relaxed);
    }
}

/// Running-action slots: at most this many show (more running actions are
/// still counted through the claimed slots).
const SLOTS: usize = 64;
/// Bytes of a slot's description (longer ones are cut).
const TEXT_WORDS: usize = 24;
const TEXT_BYTES: usize = TEXT_WORDS * 8;

/// The kinds of a slot's work.
const KIND_ACTION: u8 = 1;
const KIND_TEST: u8 = 2;

/// The strategy shown after a running action's time (0: none yet — the
/// action is checking the cache or staging its inputs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Strategy {
    Local = 1,
    LinuxSandbox = 2,
    ProcessWrapperSandbox = 3,
    RemoteCache = 4,
    Worker = 5,
    Remote = 6,
}

impl Strategy {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Local),
            2 => Some(Self::LinuxSandbox),
            3 => Some(Self::ProcessWrapperSandbox),
            4 => Some(Self::RemoteCache),
            5 => Some(Self::Worker),
            6 => Some(Self::Remote),
            _ => None,
        }
    }

    /// Bazel's strategy name (SpawnStrategyRegistry identifiers).
    pub fn name(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::LinuxSandbox => "linux-sandbox",
            Self::ProcessWrapperSandbox => "processwrapper-sandbox",
            Self::RemoteCache => "remote-cache",
            Self::Worker => "worker",
            Self::Remote => "remote",
        }
    }
}

/// One running-action slot: claimed by a worker for the duration of one
/// action, its description written under a sequence lock (odd while
/// written), so the reader thread copies a consistent one or skips it.
struct Slot {
    claimed: AtomicBool,
    sequence: AtomicU64,
    kind: AtomicU8,
    strategy: AtomicU8,
    started_ms: AtomicU64,
    len: AtomicUsize,
    text: [AtomicU64; TEXT_WORDS],
}

impl Slot {
    const fn new() -> Self {
        Self {
            claimed: AtomicBool::new(false),
            sequence: AtomicU64::new(0),
            kind: AtomicU8::new(0),
            strategy: AtomicU8::new(0),
            started_ms: AtomicU64::new(0),
            len: AtomicUsize::new(0),
            text: [const { AtomicU64::new(0) }; TEXT_WORDS],
        }
    }

    /// Write the description (the concatenation of `pieces`, cut at
    /// [`TEXT_BYTES`] on a character boundary).
    fn describe(&self, kind: u8, pieces: &[&str]) {
        let sequence = self.sequence.load(Ordering::Relaxed);
        self.sequence
            .store(sequence.wrapping_add(1), Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Release);
        let mut word = 0u64;
        let mut at = 0usize;
        'pieces: for piece in pieces {
            for character in piece.chars() {
                let mut buffer = [0u8; 4];
                let bytes = character.encode_utf8(&mut buffer).as_bytes();
                if at + bytes.len() > TEXT_BYTES {
                    break 'pieces;
                }
                for &byte in bytes {
                    word |= u64::from(byte) << ((at % 8) * 8);
                    at += 1;
                    if at.is_multiple_of(8) {
                        self.text[at / 8 - 1].store(word, Ordering::Relaxed);
                        word = 0;
                    }
                }
            }
        }
        if !at.is_multiple_of(8) {
            self.text[at / 8].store(word, Ordering::Relaxed);
        }
        self.len.store(at, Ordering::Relaxed);
        self.kind.store(kind, Ordering::Relaxed);
        self.strategy.store(0, Ordering::Relaxed);
        self.started_ms.store(now_ms(), Ordering::Relaxed);
        self.sequence
            .store(sequence.wrapping_add(2), Ordering::Release);
    }

    /// A consistent copy of a claimed slot, if one could be taken.
    fn read(&self) -> Option<render::Active> {
        if !self.claimed.load(Ordering::Acquire) {
            return None;
        }
        let before = self.sequence.load(Ordering::Acquire);
        if before % 2 == 1 {
            return None;
        }
        let len = self.len.load(Ordering::Relaxed).min(TEXT_BYTES);
        let mut bytes = Vec::with_capacity(len);
        for word in &self.text[..len.div_ceil(8)] {
            bytes.extend_from_slice(&word.load(Ordering::Relaxed).to_le_bytes());
        }
        bytes.truncate(len);
        let kind = self.kind.load(Ordering::Relaxed);
        let strategy = self.strategy.load(Ordering::Relaxed);
        let started_ms = self.started_ms.load(Ordering::Relaxed);
        std::sync::atomic::fence(Ordering::Acquire);
        if self.sequence.load(Ordering::Relaxed) != before || started_ms == 0 {
            return None;
        }
        Some(render::Active {
            description: String::from_utf8_lossy(&bytes).into_owned(),
            started_ms,
            test: kind == KIND_TEST,
            strategy: Strategy::from_u8(strategy),
        })
    }
}

static RUNNING: [Slot; SLOTS] = [const { Slot::new() }; SLOTS];

thread_local! {
    /// The slot this thread's current [`Running`] holds (usize::MAX: none).
    static CURRENT: Cell<usize> = const { Cell::new(usize::MAX) };
    /// The download this thread is streaming ([`downloaded`]).
    static DOWNLOAD: std::cell::RefCell<Option<Arc<AtomicU64>>> =
        const { std::cell::RefCell::new(None) };
}

/// A claimed running-action slot; dropping it frees the slot.
pub struct Running {
    slot: usize,
    previous: usize,
}

impl Running {
    /// An action starts: `mnemonic` and its primary output show while it
    /// runs. None when the display is off (or all slots are taken).
    #[inline]
    pub fn action(mnemonic: &str, output: &std::path::Path) -> Option<Running> {
        if !enabled() {
            return None;
        }
        let output = output.to_str().unwrap_or("");
        Self::claim(KIND_ACTION, &[mnemonic, " ", render::short_output(output)])
    }

    /// A test action starts (`Testing //pkg:test`).
    pub fn test(label: &str) -> Option<Running> {
        if !enabled() {
            return None;
        }
        Self::claim(KIND_TEST, &["Testing ", label])
    }

    fn claim(kind: u8, pieces: &[&str]) -> Option<Running> {
        for (index, slot) in RUNNING.iter().enumerate() {
            if slot
                .claimed
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                slot.describe(kind, pieces);
                let previous = CURRENT.with(|current| current.replace(index));
                return Some(Running {
                    slot: index,
                    previous,
                });
            }
        }
        None
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let slot = &RUNNING[self.slot];
        slot.started_ms.store(0, Ordering::Relaxed);
        slot.claimed.store(false, Ordering::Release);
        CURRENT.with(|current| current.set(self.previous));
    }
}

/// The current thread's running action started its spawn under `strategy`.
#[inline]
pub(crate) fn spawn_started(strategy: Strategy) {
    if !enabled() {
        return;
    }
    let index = CURRENT.with(Cell::get);
    if let Some(slot) = RUNNING.get(index) {
        slot.strategy.store(strategy as u8, Ordering::Relaxed);
    }
}

/// An in-flight repository fetch, extension evaluation or download.
struct FetchEntry {
    id: u64,
    what: String,
    started_ms: u64,
    bytes: Option<Arc<AtomicU64>>,
}

static FETCHES: Mutex<Vec<FetchEntry>> = Mutex::new(Vec::new());
static NEXT_FETCH: AtomicU64 = AtomicU64::new(1);

fn fetches() -> std::sync::MutexGuard<'static, Vec<FetchEntry>> {
    FETCHES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A registered in-flight fetch; dropping it removes it.
pub struct Fetching {
    id: u64,
    download: bool,
}

impl Fetching {
    fn register(what: String, bytes: Option<Arc<AtomicU64>>) -> Fetching {
        let id = NEXT_FETCH.fetch_add(1, Ordering::Relaxed);
        let download = bytes.is_some();
        if let Some(bytes) = &bytes {
            DOWNLOAD.with(|current| *current.borrow_mut() = Some(bytes.clone()));
        }
        fetches().push(FetchEntry {
            id,
            what,
            started_ms: now_ms(),
            bytes,
        });
        Fetching { id, download }
    }
}

impl Drop for Fetching {
    fn drop(&mut self) {
        if self.download {
            DOWNLOAD.with(|current| *current.borrow_mut() = None);
        }
        fetches().retain(|entry| entry.id != self.id);
    }
}

/// A repository rule of canonical name `canonical` executes
/// (`Fetching repository @@name`).
pub(crate) fn fetching_repository(canonical: &str) -> Option<Fetching> {
    enabled().then(|| Fetching::register(format!("repository @@{canonical}"), None))
}

/// A module extension evaluates (`Fetching module extension …`).
pub(crate) fn fetching_extension(name: impl FnOnce() -> String) -> Option<Fetching> {
    enabled().then(|| Fetching::register(format!("module extension {}", name()), None))
}

/// A download of `url` streams on this thread ([`downloaded`] counts its
/// bytes).
pub(crate) fn downloading(url: &str) -> Option<Fetching> {
    enabled().then(|| Fetching::register(url.to_owned(), Some(Arc::new(AtomicU64::new(0)))))
}

/// `count` more bytes of this thread's download arrived.
#[inline]
pub(crate) fn downloaded(count: usize) {
    if enabled() {
        DOWNLOAD.with(|current| {
            if let Some(bytes) = &*current.borrow() {
                bytes.fetch_add(count as u64, Ordering::Relaxed);
            }
        });
    }
}

/// Everything the status area shows, sampled at one point in time.
fn sample() -> render::View {
    let counters = &COUNTERS;
    let load = |counter: &AtomicUsize| counter.load(Ordering::Relaxed);
    let mut active: Vec<render::Active> = RUNNING.iter().filter_map(Slot::read).collect();
    active.sort_by_key(|entry| (entry.strategy.is_none() && !entry.test, entry.started_ms));
    let fetches = {
        let mut fetches: Vec<render::Fetch> = fetches()
            .iter()
            .map(|entry| render::Fetch {
                what: entry.what.clone(),
                started_ms: entry.started_ms,
                bytes: entry
                    .bytes
                    .as_ref()
                    .map(|bytes| bytes.load(Ordering::Relaxed)),
            })
            .collect();
        fetches.sort_by_key(|entry| entry.started_ms);
        fetches
    };
    render::View {
        phase: Phase::from_u8(counters.phase.load(Ordering::Relaxed)),
        now_ms: now_ms(),
        targets: load(&counters.targets),
        packages: load(&counters.packages),
        planned: counters
            .phase1_done
            .load(Ordering::Relaxed)
            .then(|| load(&counters.planned)),
        configured: load(&counters.configured),
        actions_total: load(&counters.actions_total),
        actions_done: load(&counters.actions_done),
        tests_total: load(&counters.tests_total),
        tests_done: load(&counters.tests_done),
        tests_failed: load(&counters.tests_failed),
        active,
        fetches,
    }
}

/// The UI options (UiOptions.java): `--curses`, `--color`,
/// `--[no]show_progress`, `--show_progress_rate_limit`,
/// `--[no]progress_in_terminal_title` and `--ui_actions_shown`.
#[derive(Clone, Debug, PartialEq)]
pub struct UiOptions {
    pub curses: Tristate,
    pub color: Tristate,
    pub show_progress: bool,
    /// Seconds.
    pub rate_limit: f64,
    pub title: bool,
    /// None: the native default (3 running actions).
    pub actions_shown: Option<usize>,
}

impl Default for UiOptions {
    fn default() -> Self {
        Self {
            curses: Tristate::Auto,
            color: Tristate::Auto,
            show_progress: true,
            rate_limit: 0.2,
            title: false,
            actions_shown: None,
        }
    }
}

/// UseCurses / UseColor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tristate {
    Yes,
    No,
    Auto,
}

impl UiOptions {
    /// Parse one UI option (`arg`, with the separate value `next`):
    /// None when `arg` is not one, else whether `next` was used.
    pub fn parse(&mut self, arg: &str, next: Option<&str>) -> Option<Result<bool, String>> {
        use crate::configuration::boolean_option;
        let valued = |name: &str| -> Option<(String, bool)> {
            let rest = arg.strip_prefix("--")?;
            if rest == name {
                return next.map(|value| (value.to_owned(), true));
            }
            rest.strip_prefix(name)?
                .strip_prefix('=')
                .map(|value| (value.to_owned(), false))
        };
        let missing = |name: &str| Err(format!("--{name} requires a value"));
        for (name, target) in [("curses", 0), ("color", 1)] {
            if arg == format!("--{name}") && next.is_none() {
                return Some(missing(name));
            }
            if let Some((value, used)) = valued(name) {
                let setting = match value.to_ascii_lowercase().as_str() {
                    "yes" => Tristate::Yes,
                    "no" => Tristate::No,
                    "auto" => Tristate::Auto,
                    _ => {
                        let shown = if used {
                            format!("--{name}={value}")
                        } else {
                            arg.to_owned()
                        };
                        return Some(Err(format!(
                            "While parsing option {shown}: Not a valid --{name} setting: '{value}' (should be yes, no or auto)"
                        )));
                    }
                };
                if target == 0 {
                    self.curses = setting;
                } else {
                    self.color = setting;
                }
                return Some(Ok(used));
            }
        }
        if let Some(value) = boolean_option(arg, "show_progress") {
            return Some(value.map(|value| {
                self.show_progress = value;
                false
            }));
        }
        if let Some(value) = boolean_option(arg, "progress_in_terminal_title") {
            return Some(value.map(|value| {
                self.title = value;
                false
            }));
        }
        if arg == "--show_progress_rate_limit" && next.is_none() {
            return Some(missing("show_progress_rate_limit"));
        }
        if let Some((value, used)) = valued("show_progress_rate_limit") {
            return Some(match value.trim().parse::<f64>() {
                Ok(seconds) if seconds.is_finite() => {
                    self.rate_limit = seconds;
                    Ok(used)
                }
                _ => Err(format!(
                    "While parsing option --show_progress_rate_limit={value}: '{value}' is not a double"
                )),
            });
        }
        for name in ["ui_actions_shown", "experimental_ui_actions_shown"] {
            if arg == format!("--{name}") && next.is_none() {
                return Some(missing(name));
            }
            if let Some((value, used)) = valued(name) {
                return Some(match value.trim().parse::<i64>() {
                    Ok(count) => {
                        // UiOptions.uiActionsShown: values below 1 show one.
                        self.actions_shown = Some(count.max(1) as usize);
                        Ok(used)
                    }
                    Err(_) => Err(format!(
                        "While parsing option --{name}={value}: '{value}' is not an int"
                    )),
                });
            }
        }
        None
    }
}

/// The environment the display decision reads ([`decide`]).
#[derive(Clone, Debug, Default)]
pub struct TerminalEnvironment {
    /// Bazel's client `--isatty`: stdout and stderr are terminals and
    /// `TERM` is a real terminal (IsStandardTerminal).
    pub standard_terminal: bool,
    /// `CI` is set (and not `false`/`0`).
    pub ci: bool,
    /// `NO_COLOR` is set and not empty (https://no-color.org).
    pub no_color: bool,
    /// `BAZEL_RS_PROGRESS=0` (or `off`/`false`/`no`).
    pub disabled: bool,
    /// The startup option `--quiet` (UiVerbosity.QUIET): no progress bar
    /// (UiEventHandler.addProgressBar returns early).
    pub quiet: bool,
    /// The width when the terminal reports none (the client's
    /// `--terminal_columns`: `$COLUMNS`, else 80).
    pub columns: usize,
}

impl TerminalEnvironment {
    /// From the process environment.
    pub fn from_process(standard_terminal: bool, columns: usize) -> Self {
        // snapshot-unrecorded: the display's own switches, read after the
        // snapshot check point decided nothing on them (never a planning input).
        let set = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        let off = |value: &str| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        };
        Self {
            standard_terminal,
            ci: set("CI").is_some_and(|value| !off(&value)),
            no_color: set("NO_COLOR").is_some(),
            disabled: set("BAZEL_RS_PROGRESS").is_some_and(|value| off(&value)),
            quiet: crate::ui::quiet(),
            columns,
        }
    }
}

/// How the display runs, when it does ([`decide`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub color: bool,
    pub rate: std::time::Duration,
    pub actions_shown: usize,
    pub title: bool,
    /// The fallback width ([`TerminalEnvironment::columns`]).
    pub columns: usize,
}

/// UiOptions.useColor: `--color=yes`, or `auto` on a standard terminal
/// (without `NO_COLOR`).
pub fn use_color(options: &UiOptions, environment: &TerminalEnvironment) -> bool {
    match options.color {
        Tristate::Yes => true,
        Tristate::No => false,
        Tristate::Auto => environment.standard_terminal && !environment.no_color,
    }
}

/// Whether the display runs for `command` under `options` in `environment`
/// (UiOptions.useCursesEnum / useColor): `--curses=yes` forces it, `auto`
/// needs a standard terminal and no `CI`; `--noshow_progress`, the startup
/// option `--quiet` and `BAZEL_RS_PROGRESS=0` turn it off (even with
/// `--curses=yes`). Only `build`, `test` and `run`.
pub fn decide(
    command: &str,
    options: &UiOptions,
    environment: &TerminalEnvironment,
) -> Option<Config> {
    if !matches!(command, "build" | "test" | "run")
        || !options.show_progress
        || environment.disabled
        || environment.quiet
    {
        return None;
    }
    let on = match options.curses {
        Tristate::Yes => true,
        Tristate::No => false,
        Tristate::Auto => environment.standard_terminal && !environment.ci,
    };
    if !on {
        return None;
    }
    let color = use_color(options, environment);
    let seconds = if options.rate_limit.is_finite() {
        options.rate_limit.clamp(0.1, 60.0)
    } else {
        0.2
    };
    Some(Config {
        color,
        rate: std::time::Duration::from_secs_f64(seconds),
        actions_shown: options.actions_shown.unwrap_or(3).max(1),
        title: options.title,
        columns: environment.columns.max(10),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_parse_like_bazel() {
        let mut options = UiOptions::default();
        assert_eq!(options.parse("--curses=yes", None), Some(Ok(false)));
        assert_eq!(options.curses, Tristate::Yes);
        assert_eq!(options.parse("--curses", Some("NO")), Some(Ok(true)));
        assert_eq!(options.curses, Tristate::No);
        assert_eq!(options.parse("--color=auto", None), Some(Ok(false)));
        assert_eq!(options.parse("--noshow_progress", None), Some(Ok(false)));
        assert!(!options.show_progress);
        assert_eq!(options.parse("--show_progress", None), Some(Ok(false)));
        assert!(options.show_progress);
        assert_eq!(
            options.parse("--show_progress_rate_limit=0.5", None),
            Some(Ok(false))
        );
        assert_eq!(options.rate_limit, 0.5);
        assert_eq!(
            options.parse("--show_progress_rate_limit", Some("1")),
            Some(Ok(true))
        );
        assert_eq!(options.parse("--ui_actions_shown=0", None), Some(Ok(false)));
        assert_eq!(options.actions_shown, Some(1));
        assert_eq!(
            options.parse("--experimental_ui_actions_shown", Some("5")),
            Some(Ok(true))
        );
        assert_eq!(options.actions_shown, Some(5));
        assert_eq!(
            options.parse("--progress_in_terminal_title", None),
            Some(Ok(false))
        );
        assert!(options.title);
        assert_eq!(
            options.parse("--curses=maybe", None),
            Some(Err(
                "While parsing option --curses=maybe: Not a valid --curses setting: 'maybe' (should be yes, no or auto)"
                    .to_owned()
            ))
        );
        assert!(
            options
                .parse("--show_progress_rate_limit=x", None)
                .unwrap()
                .is_err()
        );
        assert!(options.parse("--colors=yes", None).is_none());
        assert!(options.parse("--jobs=3", None).is_none());
        assert!(options.parse("--curses", None).unwrap().is_err());
    }

    #[test]
    fn the_decision_follows_the_terminal_and_the_flags() {
        let terminal = TerminalEnvironment {
            standard_terminal: true,
            ..Default::default()
        };
        let pipe = TerminalEnvironment::default();
        let options = UiOptions::default();
        let on = decide("build", &options, &terminal).unwrap();
        assert!(on.color);
        assert_eq!(on.rate, std::time::Duration::from_millis(200));
        assert_eq!(on.actions_shown, 3);
        assert!(decide("build", &options, &pipe).is_none());
        assert!(decide("query", &options, &terminal).is_none());
        assert!(decide("test", &options, &terminal).is_some());
        let ci = TerminalEnvironment {
            ci: true,
            ..terminal.clone()
        };
        assert!(decide("build", &options, &ci).is_none());
        let off = TerminalEnvironment {
            disabled: true,
            ..terminal.clone()
        };
        assert!(decide("run", &options, &off).is_none());
        let quiet = TerminalEnvironment {
            quiet: true,
            ..terminal.clone()
        };
        assert!(decide("build", &options, &quiet).is_none());
        let forced_quiet = UiOptions {
            curses: Tristate::Yes,
            ..UiOptions::default()
        };
        assert!(decide("build", &forced_quiet, &quiet).is_none());
        let forced = UiOptions {
            curses: Tristate::Yes,
            ..UiOptions::default()
        };
        let piped = decide("build", &forced, &pipe).unwrap();
        assert!(!piped.color);
        assert!(decide("build", &forced, &ci).is_some());
        let no = UiOptions {
            curses: Tristate::No,
            ..UiOptions::default()
        };
        assert!(decide("build", &no, &terminal).is_none());
        let quiet = UiOptions {
            show_progress: false,
            ..forced.clone()
        };
        assert!(decide("build", &quiet, &terminal).is_none());
        let no_color = TerminalEnvironment {
            no_color: true,
            ..terminal.clone()
        };
        assert!(!decide("build", &options, &no_color).unwrap().color);
        let colored = UiOptions {
            color: Tristate::Yes,
            ..UiOptions::default()
        };
        assert!(decide("build", &colored, &no_color).unwrap().color);
        let fast = UiOptions {
            rate_limit: 0.0,
            ..UiOptions::default()
        };
        assert_eq!(
            decide("build", &fast, &terminal).unwrap().rate,
            std::time::Duration::from_millis(100)
        );
    }

    #[test]
    fn slots_round_trip_their_descriptions() {
        let slot = Slot::new();
        slot.claimed.store(true, Ordering::Release);
        slot.describe(KIND_ACTION, &["GoCompilePkg", " ", "src/a/a.a"]);
        let read = slot.read().unwrap();
        assert_eq!(read.description, "GoCompilePkg src/a/a.a");
        assert!(!read.test);
        assert_eq!(read.strategy, None);
        slot.strategy
            .store(Strategy::LinuxSandbox as u8, Ordering::Relaxed);
        assert_eq!(slot.read().unwrap().strategy, Some(Strategy::LinuxSandbox));
        // Cut at the slot's size, on a character boundary.
        let long = "é".repeat(200);
        slot.describe(KIND_TEST, &["Testing ", &long]);
        let read = slot.read().unwrap();
        assert!(read.test);
        assert!(read.description.len() <= TEXT_BYTES);
        assert!(read.description.starts_with("Testing éé"));
        assert!(!read.description.contains('\u{fffd}'));
        // A slot being written is skipped.
        slot.sequence.fetch_add(1, Ordering::Relaxed);
        assert!(slot.read().is_none());
        slot.sequence.fetch_add(1, Ordering::Relaxed);
        slot.claimed.store(false, Ordering::Release);
        assert!(slot.read().is_none());
    }
}
