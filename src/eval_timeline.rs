//! `BAZEL_RS_EVAL_TIMELINE=1`: phase 1's timeline on stderr
//! (docs/EVALUATOR.md "Phase 1, measured"): marks with the process CPU
//! since the previous mark, the CPU in use over time (sampled), and the
//! sequential pass's attempts by why they did not run on the pool (with
//! their self time). Diagnostics only: nothing the analysis computes reads
//! it.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::fast_hash::FxHashMap;

static ON: LazyLock<bool> =
    // snapshot-unrecorded: a diagnostics switch; it changes nothing the analysis computes.
    LazyLock::new(|| std::env::var_os("BAZEL_RS_EVAL_TIMELINE").is_some());

/// Whether the timeline is recorded.
pub(crate) fn on() -> bool {
    *ON
}

#[derive(Default)]
struct State {
    start: Option<Instant>,
    last_cpu: u64,
    marks: Vec<(Duration, u64, String)>,
    /// Why a pool attempt left its node to the sequential pass.
    reasons: FxHashMap<u32, String>,
    /// (kind, cause) → (attempts, self µs).
    by_cause: BTreeMap<(String, String), (u64, u64)>,
    /// root cause → (attempts, self µs).
    by_root: BTreeMap<String, (u64, u64)>,
    samples: Vec<(Duration, u64)>,
    /// Named accumulators: (count, µs).
    sums: BTreeMap<&'static str, (u64, u64)>,
    /// Named intervals (relative to the start).
    intervals: Vec<(&'static str, Duration, Duration)>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(Mutex::default);
static SAMPLING: AtomicBool = AtomicBool::new(false);

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The process's CPU time (user + system) in µs.
fn cpu_us() -> u64 {
    // snapshot-unrecorded: diagnostics; the process's own CPU counters.
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return 0;
    };
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return 0;
    };
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // utime and stime are fields 14 and 15 (1-based; `rest` starts at 3).
    let ticks = |index: usize| {
        fields
            .get(index)
            .and_then(|field| field.parse::<u64>().ok())
            .unwrap_or(0)
    };
    (ticks(11) + ticks(12)) * 10_000
}

/// Start the timeline (phase 1's start) and the CPU sampler.
pub(crate) fn start() {
    if !on() {
        return;
    }
    {
        let mut state = state();
        state.start = Some(Instant::now());
        state.last_cpu = cpu_us();
    }
    if !SAMPLING.swap(true, Ordering::SeqCst) {
        let _ = std::thread::Builder::new()
            .name("eval-timeline".to_owned())
            .spawn(|| {
                while SAMPLING.load(Ordering::SeqCst) {
                    let cpu = cpu_us();
                    {
                        let mut state = state();
                        if let Some(start) = state.start {
                            let at = start.elapsed();
                            state.samples.push((at, cpu));
                        }
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            });
    }
}

/// A mark: `what` at now, with the CPU since the previous mark.
pub(crate) fn mark(what: impl FnOnce() -> String) {
    if !on() {
        return;
    }
    // Never under the timeline's lock: `what` may take other locks.
    let what = what();
    let cpu = cpu_us();
    let mut state = state();
    let Some(start) = state.start else {
        return;
    };
    let used = cpu.saturating_sub(state.last_cpu);
    state.last_cpu = cpu;
    let at = start.elapsed();
    state.marks.push((at, used, what));
}

/// Add `since`'s elapsed µs to the accumulator `name`.
pub(crate) fn add(name: &'static str, since: Instant) {
    if !on() {
        return;
    }
    let us = since.elapsed().as_micros() as u64;
    let mut state = state();
    let entry = state.sums.entry(name).or_default();
    entry.0 += 1;
    entry.1 += us;
}

/// [`add`], and the interval from `since` to now as `name`'s occupancy
/// over time in the report.
pub(crate) fn interval(name: &'static str, since: Instant) {
    if !on() {
        return;
    }
    add(name, since);
    let mut state = state();
    let Some(start) = state.start else {
        return;
    };
    let from = since.saturating_duration_since(start);
    let to = start.elapsed();
    state.intervals.push((name, from, to));
}

/// [`interval`] from the instant to its drop.
pub(crate) struct Interval(pub(crate) &'static str, pub(crate) Instant);

impl Drop for Interval {
    fn drop(&mut self) {
        interval(self.0, self.1);
    }
}

/// What the sequential pass does now (`BAZEL_RS_EVAL_WATCHDOG`).
pub(crate) static SEQ_STATE: Mutex<String> = Mutex::new(String::new());

/// Whether the watchdog is on (`BAZEL_RS_EVAL_WATCHDOG=1`).
pub(crate) fn watchdog() -> bool {
    static ON: LazyLock<bool> =
        // snapshot-unrecorded: a diagnostics switch; it changes nothing the analysis computes.
        LazyLock::new(|| std::env::var_os("BAZEL_RS_EVAL_WATCHDOG").is_some());
    *ON
}

/// What each thread blocks on now (watchdog only).
static ACTIVITIES: LazyLock<Mutex<BTreeMap<String, String>>> = LazyLock::new(Mutex::default);

/// This thread blocks on `what` until the guard drops (watchdog only).
pub(crate) fn blocked_on(what: impl FnOnce() -> String) -> Option<Blocked> {
    if !watchdog() {
        return None;
    }
    let thread = std::thread::current();
    let name = format!("{:?} {}", thread.id(), thread.name().unwrap_or("?"));
    ACTIVITIES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(name.clone(), what());
    Some(Blocked(name))
}

/// Clears a [`blocked_on`] entry when dropped.
pub(crate) struct Blocked(String);

impl Drop for Blocked {
    fn drop(&mut self) {
        ACTIVITIES
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// The threads blocked now and on what (watchdog only).
pub(crate) fn blocked_report() -> String {
    ACTIVITIES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .map(|(thread, what)| format!("\n  {thread}: {what}"))
        .collect()
}

/// Record what the sequential pass does now (watchdog only).
pub(crate) fn seq_state(what: impl FnOnce() -> String) {
    if watchdog() {
        *SEQ_STATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = what();
    }
}

/// A pool attempt of `id` left it to the sequential pass because of
/// `reason`.
pub(crate) fn note_inline(id: u32, reason: impl FnOnce() -> String) {
    if !on() {
        return;
    }
    let reason = reason();
    state().reasons.insert(id, reason);
}

/// Why the pool left `id` to the sequential pass (the last note).
pub(crate) fn inline_reason(id: u32) -> Option<String> {
    if !on() {
        return None;
    }
    state().reasons.get(&id).cloned()
}

/// Pool attempts' self time by kind per 250 ms bucket (µs).
const KINDS: &[&str] = &[
    "Pkg",
    "Tgt",
    "Src",
    "Group",
    "CT1",
    "CsMatch",
    "Platform",
    "RegToolchains",
    "RegExecPlatforms",
    "ModuleExecPlatforms",
    "ExecCfg",
    "Transition",
    "TransitionDetails",
    "FlagScope",
    "AspectPred",
    "AspectAttrAspects",
    "AspectToolchains",
    "Status",
    "CT2",
    "View",
    "Aspect",
];
const BUCKETS: usize = 400;
static POOL_BUSY: LazyLock<Vec<std::sync::atomic::AtomicU64>> = LazyLock::new(|| {
    (0..KINDS.len() * BUCKETS)
        .map(|_| std::sync::atomic::AtomicU64::new(0))
        .collect()
});

thread_local! {
    static POOL_FRAMES: std::cell::RefCell<Vec<(Instant, Duration)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// A pool attempt begins on this thread.
pub(crate) fn pool_enter() {
    POOL_FRAMES.with(|frames| frames.borrow_mut().push((Instant::now(), Duration::ZERO)));
}

/// The pool attempt begun by the last [`pool_enter`] (of `kind`) ended.
pub(crate) fn pool_exit(kind: &str) {
    let Some(own) = POOL_FRAMES.with(|frames| {
        let mut frames = frames.borrow_mut();
        let (started, children) = frames.pop()?;
        let total = started.elapsed();
        if let Some(parent) = frames.last_mut() {
            parent.1 += total;
        }
        Some(total.saturating_sub(children))
    }) else {
        return;
    };
    let Some(start) = state().start else {
        return;
    };
    let bucket = (start.elapsed().as_millis() / 250) as usize;
    let kind = KINDS.iter().position(|k| *k == kind).unwrap_or(0);
    if bucket < BUCKETS {
        POOL_BUSY[kind * BUCKETS + bucket].fetch_add(own.as_micros() as u64, Ordering::Relaxed);
    }
}

struct Frame {
    started: Instant,
    children: Duration,
    root: String,
}

thread_local! {
    static FRAMES: std::cell::RefCell<Vec<Frame>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A sequential attempt of a node of `kind` begins; `cause` is why it runs
/// here (its state before). A node reached fresh inherits its parent's
/// root cause.
pub(crate) fn seq_enter(cause: &str, detail: impl FnOnce() -> String) {
    FRAMES.with(|frames| {
        let mut frames = frames.borrow_mut();
        let root = if cause == "fresh" {
            frames
                .last()
                .map_or_else(|| "fresh (a requested root)".to_owned(), |f| f.root.clone())
        } else {
            format!("{cause}: {}", detail())
        };
        frames.push(Frame {
            started: Instant::now(),
            children: Duration::ZERO,
            root,
        });
    });
}

/// The attempt begun by the last [`seq_enter`] ended.
pub(crate) fn seq_exit(kind: &str, cause: &str) {
    let Some((own, root)) = FRAMES.with(|frames| {
        let mut frames = frames.borrow_mut();
        let frame = frames.pop()?;
        let total = frame.started.elapsed();
        if let Some(parent) = frames.last_mut() {
            parent.children += total;
        }
        Some((total.saturating_sub(frame.children), frame.root))
    }) else {
        return;
    };
    let own = own.as_micros() as u64;
    let mut state = state();
    let entry = state
        .by_cause
        .entry((kind.to_owned(), cause.to_owned()))
        .or_default();
    entry.0 += 1;
    entry.1 += own;
    let entry = state.by_root.entry(root).or_default();
    entry.0 += 1;
    entry.1 += own;
}

/// Print the timeline (phase 1's end) and stop the sampler.
pub(crate) fn report() {
    if !on() {
        return;
    }
    SAMPLING.store(false, Ordering::SeqCst);
    let state = std::mem::take(&mut *state());
    for (at, cpu, what) in &state.marks {
        crate::note!(
            "bazel-rs: eval timeline +{:>6} ms cpu {:>7.1} core-s  {what}",
            at.as_millis(),
            *cpu as f64 / 1e6
        );
    }
    // The CPU in use per 250 ms.
    let mut line = String::new();
    let mut bucket_start: Option<(Duration, u64)> = None;
    for &(at, cpu) in &state.samples {
        match bucket_start {
            None => bucket_start = Some((at, cpu)),
            Some((from, base)) if at - from >= Duration::from_millis(250) => {
                let cores = (cpu - base) as f64 / (at - from).as_micros() as f64;
                line.push_str(&format!(" {}:{cores:.1}", from.as_millis() / 100));
                bucket_start = Some((at, cpu));
            }
            Some(_) => {}
        }
    }
    crate::note!("bazel-rs: eval timeline cores in use (decisecond:cores):{line}");
    for (name, (count, us)) in &state.sums {
        crate::note!(
            "bazel-rs: eval timeline sum {name}: {count} times, {} ms",
            us / 1000
        );
    }
    for (index, kind) in KINDS.iter().enumerate() {
        let line: String = (0..BUCKETS)
            .filter_map(|bucket| {
                let us = POOL_BUSY[index * BUCKETS + bucket].load(Ordering::Relaxed);
                (us >= 25_000)
                    .then(|| format!(" {}:{:.1}", bucket * 25 / 10, us as f64 / 250_000.0))
            })
            .collect();
        if !line.is_empty() {
            crate::note!("bazel-rs: eval timeline pool {kind} self time (decisecond:cores):{line}");
        }
    }
    let mut names: Vec<&'static str> = state.intervals.iter().map(|(name, _, _)| *name).collect();
    names.sort_unstable();
    names.dedup();
    for name in names {
        // Average concurrency per 250 ms.
        let mut buckets: BTreeMap<u128, f64> = BTreeMap::new();
        for (_, from, to) in state.intervals.iter().filter(|(n, _, _)| *n == name) {
            let mut at = *from;
            while at < *to {
                let bucket = at.as_millis() / 250;
                let end = Duration::from_millis(((bucket + 1) * 250) as u64).min(*to);
                *buckets.entry(bucket).or_default() += (end - at).as_secs_f64() / 0.25;
                at = end;
            }
        }
        let line: String = buckets
            .iter()
            .map(|(bucket, load)| format!(" {}:{load:.1}", bucket * 25 / 10))
            .collect();
        crate::note!("bazel-rs: eval timeline occupancy of {name} (decisecond:concurrent):{line}");
    }
    let mut causes: Vec<_> = state.by_cause.into_iter().collect();
    causes.sort_by_key(|(_, (_, us))| std::cmp::Reverse(*us));
    for ((kind, cause), (count, us)) in causes.iter().take(60) {
        crate::note!(
            "bazel-rs: eval timeline sequential {kind} because {cause}: {count} attempts, {} ms self",
            us / 1000
        );
    }
    let mut roots: Vec<_> = state.by_root.into_iter().collect();
    roots.sort_by_key(|(_, (_, us))| std::cmp::Reverse(*us));
    for (root, (count, us)) in roots.iter().take(60) {
        crate::note!(
            "bazel-rs: eval timeline sequential subtree rooted at {root}: {count} attempts, {} ms self",
            us / 1000
        );
    }
}
