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

use super::key::NodeId;

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
    reasons: FxHashMap<NodeId, String>,
    /// (kind, cause) → (attempts, self µs).
    by_cause: BTreeMap<(String, String), (u64, u64)>,
    /// root cause → (attempts, self µs).
    by_root: BTreeMap<String, (u64, u64)>,
    samples: Vec<(Duration, u64)>,
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
    let cpu = cpu_us();
    let mut state = state();
    let Some(start) = state.start else {
        return;
    };
    let used = cpu.saturating_sub(state.last_cpu);
    state.last_cpu = cpu;
    let at = start.elapsed();
    state.marks.push((at, used, what()));
}

/// A pool attempt of `id` left it to the sequential pass because of
/// `reason`.
pub(crate) fn note_inline(id: NodeId, reason: impl FnOnce() -> String) {
    if !on() {
        return;
    }
    let reason = reason();
    state().reasons.insert(id, reason);
}

/// Why the pool left `id` to the sequential pass (the last note).
pub(crate) fn inline_reason(id: NodeId) -> Option<String> {
    if !on() {
        return None;
    }
    state().reasons.get(&id).cloned()
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
