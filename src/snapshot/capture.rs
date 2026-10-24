//! Capturing what planning printed (PLAN-L2 §1d): while a capture is
//! active, file descriptor 2 is redirected (`dup2`) into a pipe whose reader
//! thread keeps the bytes and — in [`Mode::Tee`] — writes them on to the
//! original stderr as they arrive. Every printer is covered by construction:
//! `eprintln!`, Starlark `print`, transition and `warn()` output, child
//! processes that inherit stderr. Diagnostics bypass it through
//! crate::snapshot::diag.
//!
//! * [`Mode::Tee`]: the full-analysis path of `BAZEL_RS_SNAPSHOT=on` from
//!   the snapshot check point to the end of planning; the kept bytes are the
//!   snapshot's window stderr, which a hit replays.
//! * [`Mode::Discard`]: a hit attempt. Validation never prints, but a
//!   replayed local repository rule's definition load evaluates its `.bzl`
//!   closure again with the loader's default print handler: such a print is
//!   already part of the recorded window bytes, so it is swallowed.
//! * [`Mode::Shadow`]: verify mode's shadow full analysis of a hit
//!   (crate::snapshot::verify): kept to be compared, never printed (the hit
//!   prints its own bytes).
//!
//! Stopping restores the original stderr first, then waits (bounded) until
//! the reader saw the end of the pipe, so every byte written before the stop
//! is on the original stderr before anything written after it. A process
//! that still holds the pipe (a lingering child) keeps the reader alive: it
//! is detached — it keeps forwarding — and the capture is incomplete, which
//! keeps the run from being saved. A panic while any capture is active
//! restores stderr before the panic message prints (a [`Mode::Tee`] or
//! [`Mode::Shadow`] capture is then marked broken; a [`Mode::Discard`] one —
//! a hit attempt, which catches the panic and misses — is not: L2-W5f review
//! F6, the message is seen instead of swallowed); the memory watchdog does
//! the same before it exits.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Once, PoisonError};
use std::time::Duration;

/// Window stderr kept at most (beyond it the capture overflows: the run is
/// not saved; the bytes are still forwarded).
pub const LIMIT: usize = 16 << 20;

/// What a capture does with the bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Forward to the original stderr and keep them.
    Tee,
    /// Keep them only (a hit attempt prints nothing).
    Discard,
    /// Keep them only, for a whole full analysis (verify mode's shadow
    /// analysis, crate::snapshot::verify: what it printed is compared, the
    /// hit prints its own bytes): like [`Mode::Tee`], a panic or the memory
    /// watchdog's exit restores stderr first, so their messages are seen.
    Shadow,
}

impl Mode {
    /// Whether ending the capture early (a panic, the watchdog's exit)
    /// breaks it: the window's bytes are incomplete, the run is not saved.
    /// A hit attempt's capture ([`Mode::Discard`]) holds no window bytes.
    fn breaks_when_restored(self) -> bool {
        matches!(self, Self::Tee | Self::Shadow)
    }
}

#[derive(Default)]
struct DrainState {
    finished: bool,
    bytes: Vec<u8>,
    overflowed: bool,
}

#[derive(Default)]
struct Drained {
    state: Mutex<DrainState>,
    finished: Condvar,
}

struct Active {
    original: Arc<File>,
    mode: Mode,
    drained: Arc<Drained>,
}

static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);
/// A [`Mode::Tee`] capture ended early: a panic while it was active, or the
/// memory watchdog's exit.
static BROKEN: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK: Once = Once::new();

fn active() -> std::sync::MutexGuard<'static, Option<Active>> {
    ACTIVE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The token of the active capture; [`Capture::stop`] ends it (dropping it
/// stops it too, discarding the bytes).
#[derive(Debug)]
pub struct Capture {
    stopped: bool,
}

/// What a stopped capture kept.
#[derive(Debug, Default)]
pub struct Captured {
    pub bytes: Vec<u8>,
    /// The reader saw the end of the pipe within the bound: nothing written
    /// before the stop is missing, and no process still holds the pipe.
    pub complete: bool,
    /// More than [`LIMIT`] bytes were written (only the first are kept).
    pub overflowed: bool,
}

/// Redirect file descriptor 2 into a capture (one at a time per process).
pub fn start(mode: Mode) -> io::Result<Capture> {
    let mut slot = active();
    if slot.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a stderr capture is already active",
        ));
    }
    let original = Arc::new(File::from(io::stderr().as_fd().try_clone_to_owned()?));
    let (mut reader, writer) = io::pipe()?;
    let drained = Arc::new(Drained::default());
    let forward = (mode == Mode::Tee).then(|| original.clone());
    let thread_drained = drained.clone();
    std::thread::Builder::new()
        .name("stderr-capture".to_owned())
        .spawn(move || {
            let mut chunk = vec![0u8; 64 << 10];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(count) => {
                        if let Some(original) = &forward {
                            let _ = (&**original).write_all(&chunk[..count]);
                        }
                        let mut state = thread_drained
                            .state
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        if state.bytes.len() + count <= LIMIT {
                            state.bytes.extend_from_slice(&chunk[..count]);
                        } else {
                            state.overflowed = true;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            let mut state = thread_drained
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            state.finished = true;
            thread_drained.finished.notify_all();
        })?;
    // On failure the writer drops here: the reader sees the end of the pipe.
    rustix::stdio::dup2_stderr(&writer).map_err(io::Error::from)?;
    drop(writer);
    crate::snapshot::diag::set_original(Some(original.clone()));
    install_panic_hook();
    *slot = Some(Active {
        original,
        mode,
        drained,
    });
    Ok(Capture { stopped: false })
}

impl Capture {
    /// Restore the original stderr and collect the bytes, waiting at most
    /// `bound` for the reader to see the end of the pipe.
    pub fn stop(mut self, bound: Duration) -> Captured {
        self.stopped = true;
        finish(bound).unwrap_or_default()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = finish(Duration::from_millis(500));
        }
    }
}

fn finish(bound: Duration) -> Option<Captured> {
    let active = active().take()?;
    let _ = rustix::stdio::dup2_stderr(&*active.original);
    crate::snapshot::diag::set_original(None);
    let state = active
        .drained
        .state
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let (mut state, _) = active
        .drained
        .finished
        .wait_timeout_while(state, bound, |state| !state.finished)
        .unwrap_or_else(PoisonError::into_inner);
    let complete = state.finished;
    let bytes = if complete {
        std::mem::take(&mut state.bytes)
    } else {
        // Detached: the reader keeps forwarding (and appending).
        state.bytes.clone()
    };
    Some(Captured {
        bytes,
        complete,
        overflowed: state.overflowed,
    })
}

/// The mode of the active capture, if any.
fn active_mode() -> Option<Mode> {
    active().as_ref().map(|active| active.mode)
}

/// End the active capture now, before the process exits or a panic message
/// prints: restore the original stderr and let the reader forward what is
/// still in the pipe (briefly bounded). A [`Mode::Tee`] or [`Mode::Shadow`]
/// capture is broken: the run is not saved.
pub fn restore_for_exit() {
    let Some(mode) = active_mode() else {
        return;
    };
    if mode.breaks_when_restored() {
        BROKEN.store(true, Ordering::SeqCst);
    }
    let _ = finish(Duration::from_millis(200));
}

/// Whether a [`Mode::Tee`] capture of this process ended early
/// ([`restore_for_exit`]).
pub fn broken() -> bool {
    BROKEN.load(Ordering::SeqCst)
}

fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_for_exit();
            previous(info);
        }));
    });
}
