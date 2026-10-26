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
//! is on the original stderr before anything written after it. Forwarded
//! bytes go out whole lines at a time under the diagnostics stream's order
//! lock (crate::snapshot::diag::ORDER, L2-W5h): a diagnostics line never
//! lands inside a forwarded line (a partial line waits for its newline or
//! the end of the pipe — L2-W5i: at most [`PENDING_LIMIT`] bytes of it). A
//! process that still holds the pipe (a lingering child) keeps the reader
//! alive: the stop forwards the partial line it holds back at once (L2-W5i;
//! before, it waited for a newline or the end of the pipe, and was lost if
//! the process ended first), the reader is detached — it forwards what
//! still arrives as it arrives — and the capture is incomplete, which keeps
//! the run from being saved. A panic while any capture is active
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
/// Bytes of a partial line the reader holds back for its newline at most
/// (L2-W5i): a longer one is forwarded as it is.
pub const PENDING_LIMIT: usize = 64 << 10;

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
    /// Bytes to forward, up to their last newline (L2-W5h: whole lines;
    /// L2-W5i: here, not in the reader, so a stop can flush them).
    pending: Vec<u8>,
    /// The stop found the capture incomplete — a process still holds the
    /// pipe — and flushed the partial line (L2-W5i): from then on the
    /// detached reader forwards bytes as they arrive (before, a partial line
    /// waited for its newline or the end of the pipe, and was lost when the
    /// process ended first).
    detached: bool,
    /// What is forwarded passes through it (crate::ui: the marks of the
    /// notes are stripped, under `--quiet` the notes too); the kept bytes
    /// keep them.
    filter: Option<crate::ui::Filter>,
}

impl DrainState {
    /// `bytes` as the console shows them (the filter's state carries over
    /// from the bytes forwarded before).
    fn console(&mut self, bytes: &[u8]) -> Vec<u8> {
        let filter = self
            .filter
            .get_or_insert_with(|| crate::ui::Filter::new(crate::ui::quiet()));
        let mut out = Vec::with_capacity(bytes.len());
        filter.feed(bytes, &mut out);
        out
    }
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
/// Whether file descriptor 2 is redirected into a capture's pipe (read
/// without the lock by crate::ui's writers of notes).
static REDIRECTED: AtomicBool = AtomicBool::new(false);
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
    /// The capture ended before its owner stopped it — a panic (caught
    /// later, or the process would have ended) or the memory watchdog's
    /// exit ([`restore_for_exit`]) — so `bytes` is empty and `complete`
    /// false for that reason, not a lingering child (L2-W5h).
    pub ended_early: bool,
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
    spawn_reader(
        reader,
        original.clone(),
        mode,
        drained.clone(),
        LIMIT,
        PENDING_LIMIT,
    )?;
    // On failure the writer drops here: the reader sees the end of the pipe.
    rustix::stdio::dup2_stderr(&writer).map_err(io::Error::from)?;
    drop(writer);
    REDIRECTED.store(true, Ordering::SeqCst);
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
/// `mode` (see [`Mode`]) until the pipe ends. What it forwards goes out
/// whole lines at a time under crate::snapshot::diag's order lock (L2-W5h):
/// a partial line waits for its newline — or the end of the pipe — so a
/// diagnostics line never lands inside a forwarded one; L2-W5i: at most
/// `line_limit` bytes of it (a longer partial line is forwarded as it is),
/// and an incomplete stop flushes it ([`flush_pending`]). It never writes
/// while holding the drain state's lock, on which the stop waits (bounded;
/// L2-W5h: before, a Shadow forward wrote under it).
fn spawn_reader(
    mut reader: io::PipeReader,
    original: Arc<File>,
    mode: Mode,
    drained: Arc<Drained>,
    limit: usize,
    line_limit: usize,
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
                        let forward = {
                            let mut state = drained.lock();
                            let forward = if state.forwarding {
                                // Shadow, past the switch: through, after
                                // the kept bytes ([`forward_kept`] wrote
                                // them under the order lock first).
                                true
                            } else if state.bytes.len() + count <= limit {
                                state.bytes.extend_from_slice(chunk);
                                mode == Mode::Tee
                            } else {
                                state.overflowed = true;
                                if mode == Mode::Shadow {
                                    // Never comparable now (a mismatch):
                                    // what the shadow analysis prints is
                                    // the output — the kept bytes first.
                                    state.forwarding = true;
                                    let kept = state.bytes.clone();
                                    state.pending.extend_from_slice(&kept);
                                    true
                                } else {
                                    mode == Mode::Tee
                                }
                            };
                            if forward {
                                state.pending.extend_from_slice(chunk);
                            }
                            forward
                        };
                        if forward {
                            forward_pending(&drained, &original, false, line_limit);
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            forward_pending(&drained, &original, true, line_limit);
            let mut state = drained.lock();
            state.finished = true;
            drained.finished.notify_all();
        })
        .map(drop)
}

/// Write the pending bytes' whole lines — everything with `all` (the end of
/// the pipe), once detached ([`DrainState::detached`]) or when the partial
/// line is longer than `line_limit` — to `original` and drop them. The
/// order lock is taken before the state's lock and held across the write:
/// a chunk taken later never goes out before one taken earlier (the reader
/// and [`flush_pending`] both forward).
fn forward_pending(drained: &Drained, original: &File, all: bool, line_limit: usize) {
    let _order = crate::snapshot::diag::ORDER
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let bytes = {
        let mut state = drained.lock();
        let end = if all || state.detached || state.pending.len() > line_limit {
            state.pending.len()
        } else {
            state
                .pending
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |at| at + 1)
        };
        let bytes = state.pending.drain(..end).collect::<Vec<u8>>();
        let mut bytes = state.console(&bytes);
        if all && let Some(filter) = state.filter.as_mut() {
            filter.finish(&mut bytes);
        }
        if bytes.is_empty() {
            return;
        }
        bytes
    };
    let mut original = original;
    let _ = original.write_all(&bytes).and_then(|()| original.flush());
}

/// An incomplete stop (a process still holds the pipe, L2-W5i): forward the
/// partial line the reader holds back now — it would wait for a newline or
/// the end of the pipe, and be lost if the process ended first — and let
/// the detached reader forward what still arrives as it arrives. `early` (a
/// panic hook, the watchdog's exit) waits for the order lock briefly only;
/// without it the flag still lets the reader forward the rest.
fn flush_pending(drained: &Drained, original: &File, early: bool) {
    let order = if early {
        crate::snapshot::diag::order_within(Duration::from_millis(50))
    } else {
        Some(
            crate::snapshot::diag::ORDER
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    };
    let bytes = {
        let mut state = drained.lock();
        state.detached = true;
        if order.is_none() {
            return;
        }
        let bytes = std::mem::take(&mut state.pending);
        state.console(&bytes)
    };
    if !bytes.is_empty() {
        let mut original = original;
        let _ = original.write_all(&bytes).and_then(|()| original.flush());
    }
    drop(order);
}

/// [`Mode::Shadow`]: write the kept bytes to the original stderr (once) and
/// switch the reader to forwarding. The order lock is taken first and held
/// across the switch and the write — never the state's lock while writing
/// — so no forwarded chunk can come before the kept bytes; `early` (a panic
/// hook, the watchdog's exit) waits for the order lock briefly only.
fn forward_kept(drained: &Drained, original: &File, early: bool) {
    let order = if early {
        crate::snapshot::diag::order_within(Duration::from_millis(50))
    } else {
        Some(
            crate::snapshot::diag::ORDER
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    };
    let kept = {
        let mut state = drained.lock();
        if state.forwarding {
            // The reader switched first (an overflow) and forwards them.
            return;
        }
        state.forwarding = true;
        let kept = state.bytes.clone();
        state.console(&kept)
    };
    let mut original = original;
    let _ = original.write_all(&kept).and_then(|()| original.flush());
    drop(order);
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
            ended_early: true,
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
    REDIRECTED.store(false, Ordering::SeqCst);
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
    let complete = {
        let state = drained.lock();
        let (state, _) = drained
            .finished
            .wait_timeout_while(state, bound, |state| !state.finished)
            .unwrap_or_else(PoisonError::into_inner);
        state.finished
    };
    if mode == Mode::Shadow && (early || !complete) {
        forward_kept(drained, original, early);
    }
    if !complete {
        // A lingering child holds the pipe: its partial line goes out now
        // (L2-W5i), and the detached reader forwards the rest as it comes.
        flush_pending(drained, original, early);
    }
    let mut state = drained.lock();
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
        ended_early: early,
    }
}

/// Whether a capture holds file descriptor 2 now (crate::ui writes notes
/// marked into it).
pub fn is_active() -> bool {
    REDIRECTED.load(Ordering::Relaxed)
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
        rig_with(mode, limit, PENDING_LIMIT)
    }

    fn rig_with(mode: Mode, limit: usize, line_limit: usize) -> Rig {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("original");
        let original = Arc::new(File::create(&path).unwrap());
        let (reader, writer) = io::pipe().unwrap();
        let drained = Arc::new(Drained::default());
        spawn_reader(
            reader,
            original.clone(),
            mode,
            drained.clone(),
            limit,
            line_limit,
        )
        .unwrap();
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
        assert!(!captured.ended_early);
        assert!(rig.original().is_empty());
    }

    /// L2-W5g: a shadow capture over its limit forwards what it kept, then
    /// everything after it, in order (whole lines, L2-W5h).
    #[test]
    fn a_shadow_capture_forwards_once_it_overflows() {
        let mut rig = rig(Mode::Shadow, 8);
        rig.write(b"0123");
        rig.settle(|state| state.bytes.len() == 4);
        rig.write(b"4567\n9");
        rig.wait_original(b"01234567\n");
        rig.write(b"xyz\n");
        rig.wait_original(b"01234567\n9xyz\n");
        let captured = rig.stop(true, false);
        assert!(captured.complete && captured.overflowed && captured.forwarded);
        assert_eq!(rig.original(), b"01234567\n9xyz\n");
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
        rig.write(b"def\n");
        rig.wait_original(b"abcdef\n");
        rig.writer = None;
        rig.settle(|state| state.finished);
    }

    /// L2-W5h: what a capture forwards goes out whole lines at a time (a
    /// diagnostics line, written under the same order lock, never lands
    /// inside one): a partial line waits for its newline or the end of the
    /// pipe; the kept bytes are what arrived, as it arrived.
    #[test]
    fn forwarded_bytes_go_out_in_whole_lines() {
        let mut rig = rig(Mode::Tee, 64);
        rig.write(b"rules.bzl loaded");
        rig.settle(|state| state.bytes.len() == 16);
        std::thread::sleep(Duration::from_millis(50));
        assert!(rig.original().is_empty());
        rig.write(b"\nmacro for root\npart");
        rig.wait_original(b"rules.bzl loaded\nmacro for root\n");
        let captured = rig.stop(true, false);
        assert_eq!(rig.original(), b"rules.bzl loaded\nmacro for root\npart");
        assert_eq!(captured.bytes, b"rules.bzl loaded\nmacro for root\npart");
        assert!(captured.complete && captured.forwarded);
    }

    /// L2-W5i: a partial line longer than the limit is forwarded as it is
    /// (the reader holds back at most that much), and an incomplete stop —
    /// a lingering child holds the pipe — forwards the partial line at once
    /// and every later byte as it arrives (before, it waited for a newline
    /// or the end of the pipe).
    #[test]
    fn partial_lines_are_bounded_and_flushed_by_an_incomplete_stop() {
        let mut tee = rig_with(Mode::Tee, 64, 8);
        tee.write(b"short");
        tee.settle(|state| state.bytes.len() == 5);
        std::thread::sleep(Duration::from_millis(50));
        assert!(tee.original().is_empty());
        tee.write(b" and much longer");
        tee.wait_original(b"short and much longer");
        let captured = tee.stop(true, false);
        assert!(captured.complete);
        let mut tee = rig(Mode::Tee, 64);
        tee.write(b"line\npartial");
        tee.wait_original(b"line\n");
        tee.settle(|state| state.pending == b"partial");
        let captured = tee.stop(false, false);
        assert!(!captured.complete && captured.forwarded);
        assert_eq!(tee.original(), b"line\npartial");
        tee.write(b" more");
        tee.wait_original(b"line\npartial more");
        tee.writer = None;
        tee.settle(|state| state.finished);
        assert_eq!(tee.original(), b"line\npartial more");
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
        // L2-W5h: an early end says so (a caught panic is no lingering
        // child).
        assert!(captured.ended_early);
        assert_eq!(rig.original(), b"before the panic\n");
    }

    #[test]
    fn tee_forwards_everything_and_discard_nothing() {
        let mut tee = rig(Mode::Tee, 8);
        tee.write(b"01234567\n9");
        tee.wait_original(b"01234567\n");
        let captured = tee.stop(true, true);
        assert!(captured.overflowed && captured.forwarded);
        assert_eq!(tee.original(), b"01234567\n9");
        let mut discard = rig(Mode::Discard, 8);
        discard.write(b"0123456789");
        discard.settle(|state| state.overflowed);
        let captured = discard.stop(true, true);
        assert!(captured.overflowed && !captured.forwarded);
        assert!(discard.original().is_empty());
    }
}
