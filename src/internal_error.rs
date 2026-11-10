//! A panic is an internal error: the command ends at once with Bazel's
//! internal-error exit code 37 (ExitCode.BLAZE_INTERNAL_ERROR, Bazel's
//! `FATAL: bazel crashed due to an internal error`), whichever thread
//! panicked, instead of unwinding. Unwinding the coordinator ran the drops
//! of pools whose peers wait for it (a pool joined while a handle of it is
//! held further up the stack), and a panic on a thread nobody joins (the
//! planning drop thread) was never noticed; neither can hang or pass now.
//!
//! The few panics that are recovered by design stay recoverable: those
//! inside [`recoverable`] on the same thread (a damaged snapshot's decoder,
//! a speculation that the demand path redoes), and an analysis worker's
//! (crate::engine::parallel's own hook records it; the coordinator panics
//! with it at the target's turn, which ends here).

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, UnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Bazel's ExitCode.BLAZE_INTERNAL_ERROR.
pub const EXIT_CODE: i32 = 37;

thread_local! {
    /// How many [`recoverable`] calls this thread is inside.
    static RECOVERABLE: Cell<u32> = const { Cell::new(0) };
}

/// Install (once, first thing in `main`) the panic hook that ends the
/// process with [`EXIT_CODE`] after printing the panic (the previous
/// hook's message and backtrace).
pub fn install() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if RECOVERABLE.with(Cell::get) > 0 {
                previous(info);
                return;
            }
            if !claim() {
                // Another thread is ending the process.
                loop {
                    std::thread::park();
                }
            }
            eprintln!(
                "FATAL: bazel-rs crashed due to an internal error (exit code {EXIT_CODE}). Printing the panic:"
            );
            previous(info);
            if EVALUATOR.load(Ordering::SeqCst) > 0 {
                eprintln!(
                    "bazel-rs: the panic happened while the parallel analysis evaluator (the default engine) planned; rerun with BAZEL_RS_ANALYSIS=walk to plan with the sequential walk"
                );
            }
            crash();
        }));
    });
}

/// `work`, its panic returned instead of ending the process (the caller
/// recovers from it). Only for work whose panic the command recovers from
/// by design; threads `work` starts are not covered.
pub fn recoverable<R>(work: impl FnOnce() -> R + UnwindSafe) -> std::thread::Result<R> {
    struct Depth;
    impl Drop for Depth {
        fn drop(&mut self) {
            RECOVERABLE.with(|depth| depth.set(depth.get() - 1));
        }
    }
    RECOVERABLE.with(|depth| depth.set(depth.get() + 1));
    let _depth = Depth;
    std::panic::catch_unwind(work)
}

/// [`recoverable`] for work that is not [`UnwindSafe`] (what it touched
/// is discarded or checked by the caller).
pub fn recoverable_asserted<R>(work: impl FnOnce() -> R) -> std::thread::Result<R> {
    recoverable(AssertUnwindSafe(work))
}

/// `work` on a thread of its own whose panic its joiner recovers from
/// (the join's `Err`): [`recoverable`], the panic resumed without the hook.
pub fn recoverable_resumed<R>(work: impl FnOnce() -> R) -> R {
    recoverable_asserted(work).unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// How many [`EvaluatorScope`]s are open (any thread).
static EVALUATOR: AtomicU32 = AtomicU32::new(0);

/// While open, a panic (on any thread) names `BAZEL_RS_ANALYSIS=walk` as
/// the workaround: the planning runs on the evaluator
/// (crate::engine::eval::select).
pub struct EvaluatorScope(());

impl EvaluatorScope {
    pub fn enter() -> Self {
        EVALUATOR.fetch_add(1, Ordering::SeqCst);
        Self(())
    }
}

impl Drop for EvaluatorScope {
    fn drop(&mut self) {
        EVALUATOR.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Whether this thread is the first to crash (the one that ends the
/// process; a later panicking thread waits for that exit).
fn claim() -> bool {
    static CRASHING: AtomicBool = AtomicBool::new(false);
    !CRASHING.swap(true, Ordering::SeqCst)
}

/// End the process with [`EXIT_CODE`].
fn crash() -> ! {
    // As the memory watchdog's exit: no action outlives the process, and
    // what a stderr capture or the progress display holds is written.
    crate::memory_limit::kill_live_children();
    crate::snapshot::capture::restore_for_exit();
    crate::progress::abandon();
    std::process::exit(EXIT_CODE)
}
