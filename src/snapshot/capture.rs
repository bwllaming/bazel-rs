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
//!   prints its own bytes) — unless they can no longer be compared (L2-W5g):
//!   when the capture overflows its [`LIMIT`], ends early (a panic, the
//!   memory watchdog's exit) or stops incomplete (a process still holds the
//!   pipe), the kept bytes are written to the original stderr at once and
//!   every later byte follows them there ([`Captured::forwarded`]). Each of
//!   those is a mismatch (or the end of the process), whose output is the
//!   full analysis's: before, those bytes were lost.
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
    /// watchdog's exit restores stderr first, so their messages are seen —
    /// and the kept bytes are forwarded then, as on an overflow or an
    /// incomplete stop (L2-W5g).
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
    /// [`Mode::Shadow`]: the kept bytes were written to the original stderr,
    /// and every later chunk goes straight there ([`forward_kept`]).
    forwarding: bool,
}

#[derive(Default)]
struct Drained {
    state: Mutex<DrainState>,
    finished: Condvar,
}

impl Drained {
    fn lock(&self) -> std::sync::MutexGuard<'_, DrainState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
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
    /// Every byte written into the capture is on the original stderr
    /// already, so the caller must not print `bytes` again: always for
    /// [`Mode::Tee`]; for [`Mode::Shadow`] once it overflowed, ended early
    /// or stopped incomplete (L2-W5g).
    pub forwarded: bool,
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
    let (reader, writer) = io::pipe()?;
    let drained = Arc::new(Drained::default());
    spawn_reader(reader, original.clone(), mode, drained.clone(), LIMIT)?;
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

/// The reader thread of a capture: keeps up to `limit` bytes, forwards per
/// `mode` (see [`Mode`]) until the pipe ends.
fn spawn_reader(
    mut reader: io::PipeReader,
    original: Arc<File>,
    mode: Mode,
    drained: Arc<Drained>,
    limit: usize,
) -> io::Result<()> {
    std::thread::Builder::new()
        .name("stderr-capture".to_owned())
        .spawn(move || {
            let mut chunk = vec![0u8; 64 << 10];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(count) => {
                        let chunk = &chunk[..count];
                        if mode == Mode::Tee {
                            let _ = (&*original).write_all(chunk);
                        }
                        let mut state = drained.lock();
                        if state.forwarding {
                            // Shadow, past the switch: straight through,
                            // in order after the kept bytes (both are
                            // written under the state's lock).
                            let _ = (&*original).write_all(chunk);
                        } else if state.bytes.len() + count <= limit {
                            state.bytes.extend_from_slice(chunk);
                        } else {
                            state.overflowed = true;
                            if mode == Mode::Shadow {
                                // Never comparable now (a mismatch): what
                                // the shadow analysis prints is the output.
                                forward_kept(&mut state, &original);
                                let _ = (&*original).write_all(chunk);
                            }
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            let mut state = drained.lock();
            state.finished = true;
            drained.finished.notify_all();
        })
        .map(drop)
}

/// [`Mode::Shadow`]: write the kept bytes to the original stderr (once) and
/// switch the reader to forwarding. Called under the state's lock, so no
/// chunk can come between the kept bytes and the forwarded ones.
fn forward_kept(state: &mut DrainState, original: &File) {
    if !state.forwarding {
        state.forwarding = true;
        let mut original = original;
        let _ = original
            .write_all(&state.bytes)
            .and_then(|()| original.flush());
    }
}

impl Capture {
    /// Restore the original stderr and collect the bytes, waiting at most
    /// `bound` for the reader to see the end of the pipe.
    pub fn stop(mut self, bound: Duration) -> Captured {
        self.stopped = true;
        // None: the capture already ended early ([`restore_for_exit`]: a
        // caught panic), which forwarded what it kept (Tee, Shadow) and
        // restored stderr: what came after went there directly.
        finish(bound, false).unwrap_or(Captured {
            forwarded: true,
            ..Captured::default()
        })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = finish(Duration::from_millis(500), false);
        }
    }
}

/// End the active capture: restore the original stderr, then collect.
/// `early`: the capture ends before its owner stops it (a panic, the
/// watchdog's exit).
fn finish(bound: Duration, early: bool) -> Option<Captured> {
    let active = active().take()?;
    let _ = rustix::stdio::dup2_stderr(&*active.original);
    crate::snapshot::diag::set_original(None);
    Some(collect(
        &active.drained,
        &active.original,
        active.mode,
        bound,
        early,
    ))
}

/// Wait at most `bound` for the reader to see the end of the pipe, then
/// take what it kept. A [`Mode::Shadow`] capture that ends `early` or
/// incomplete forwards its kept bytes ([`forward_kept`]); a detached reader
/// keeps forwarding what still arrives.
fn collect(
    drained: &Drained,
    original: &File,
    mode: Mode,
    bound: Duration,
    early: bool,
) -> Captured {
    let state = drained.lock();
    let (mut state, _) = drained
        .finished
        .wait_timeout_while(state, bound, |state| !state.finished)
        .unwrap_or_else(PoisonError::into_inner);
    let complete = state.finished;
    if mode == Mode::Shadow && (early || !complete) {
        forward_kept(&mut state, original);
    }
    let bytes = if complete {
        std::mem::take(&mut state.bytes)
    } else {
        // Detached: the reader keeps forwarding (Tee: and appending).
        state.bytes.clone()
    };
    Captured {
        bytes,
        complete,
        overflowed: state.overflowed,
        forwarded: mode == Mode::Tee || state.forwarding,
    }
}

/// The mode of the active capture, if any.
fn active_mode() -> Option<Mode> {
    active().as_ref().map(|active| active.mode)
}

/// End the active capture now, before the process exits or a panic message
/// prints: restore the original stderr and let the reader forward what is
/// still in the pipe (briefly bounded); a [`Mode::Shadow`] capture forwards
/// what it kept (L2-W5g). A [`Mode::Tee`] or [`Mode::Shadow`] capture is
/// broken: the run is not saved.
pub fn restore_for_exit() {
    let Some(mode) = active_mode() else {
        return;
    };
    if mode.breaks_when_restored() {
        BROKEN.store(true, Ordering::SeqCst);
    }
    let _ = finish(Duration::from_millis(200), true);
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

#[cfg(test)]
mod tests {
    //! The reader and the stop on a private pipe and a file standing in for
    //! the original stderr (file descriptor 2 is never touched here).
    use super::*;
    use std::time::Instant;

    struct Rig {
        writer: Option<io::PipeWriter>,
        original: Arc<File>,
        path: std::path::PathBuf,
        drained: Arc<Drained>,
        mode: Mode,
        _temp: tempfile::TempDir,
    }

    fn rig(mode: Mode, limit: usize) -> Rig {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("original");
        let original = Arc::new(File::create(&path).unwrap());
        let (reader, writer) = io::pipe().unwrap();
        let drained = Arc::new(Drained::default());
        spawn_reader(reader, original.clone(), mode, drained.clone(), limit).unwrap();
        Rig {
            writer: Some(writer),
            original,
            path,
            drained,
            mode,
            _temp: temp,
        }
    }

    impl Rig {
        fn write(&mut self, bytes: &[u8]) {
            self.writer.as_mut().unwrap().write_all(bytes).unwrap();
        }

        /// Wait until the reader consumed `kept` bytes (or forwarded them).
        fn settle(&self, until: impl Fn(&DrainState) -> bool) {
            let started = Instant::now();
            while !until(&self.drained.lock()) {
                assert!(started.elapsed() < Duration::from_secs(10), "reader stuck");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn original(&self) -> Vec<u8> {
            std::fs::read(&self.path).unwrap()
        }

        fn wait_original(&self, expected: &[u8]) {
            let started = Instant::now();
            while self.original() != expected {
                assert!(
                    started.elapsed() < Duration::from_secs(10),
                    "original stderr: {:?}",
                    String::from_utf8_lossy(&self.original())
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn stop(&mut self, close: bool, early: bool) -> Captured {
            if close {
                self.writer = None;
            }
            collect(
                &self.drained,
                &self.original,
                self.mode,
                Duration::from_millis(if close { 5000 } else { 50 }),
                early,
            )
        }
    }

    #[test]
    fn a_shadow_capture_keeps_its_bytes_when_it_stops_complete() {
        let mut rig = rig(Mode::Shadow, 64);
        rig.write(b"planning printed\n");
        let captured = rig.stop(true, false);
        assert_eq!(captured.bytes, b"planning printed\n");
        assert!(captured.complete && !captured.overflowed && !captured.forwarded);
        assert!(rig.original().is_empty());
    }

    /// L2-W5g: a shadow capture over its limit forwards what it kept, then
    /// everything after it, in order.
    #[test]
    fn a_shadow_capture_forwards_once_it_overflows() {
        let mut rig = rig(Mode::Shadow, 8);
        rig.write(b"0123");
        rig.settle(|state| state.bytes.len() == 4);
        rig.write(b"456789");
        rig.wait_original(b"0123456789");
        rig.write(b"xyz");
        rig.wait_original(b"0123456789xyz");
        let captured = rig.stop(true, false);
        assert!(captured.complete && captured.overflowed && captured.forwarded);
        assert_eq!(rig.original(), b"0123456789xyz");
    }

    /// L2-W5g: a shadow capture stopped while a process still holds the
    /// pipe forwards what it kept; the detached reader forwards the rest.
    #[test]
    fn an_incomplete_shadow_stop_forwards_the_kept_and_later_bytes() {
        let mut rig = rig(Mode::Shadow, 64);
        rig.write(b"abc");
        rig.settle(|state| state.bytes.len() == 3);
        let captured = rig.stop(false, false);
        assert!(!captured.complete && captured.forwarded);
        assert_eq!(captured.bytes, b"abc");
        assert_eq!(rig.original(), b"abc");
        rig.write(b"def");
        rig.wait_original(b"abcdef");
        rig.writer = None;
        rig.settle(|state| state.finished);
    }

    /// L2-W5g: a shadow capture ended early (a panic, the memory watchdog's
    /// exit) forwards what it kept.
    #[test]
    fn a_shadow_capture_ended_early_forwards_its_bytes() {
        let mut rig = rig(Mode::Shadow, 64);
        rig.write(b"before the panic\n");
        rig.settle(|state| !state.bytes.is_empty());
        let captured = rig.stop(true, true);
        assert!(captured.forwarded);
        assert_eq!(rig.original(), b"before the panic\n");
    }

    #[test]
    fn tee_forwards_everything_and_discard_nothing() {
        let mut tee = rig(Mode::Tee, 8);
        tee.write(b"0123456789");
        tee.wait_original(b"0123456789");
        let captured = tee.stop(true, true);
        assert!(captured.overflowed && captured.forwarded);
        assert_eq!(tee.original(), b"0123456789");
        let mut discard = rig(Mode::Discard, 8);
        discard.write(b"0123456789");
        discard.settle(|state| state.overflowed);
        let captured = discard.stop(true, true);
        assert!(captured.overflowed && !captured.forwarded);
        assert!(discard.original().is_empty());
    }
}
