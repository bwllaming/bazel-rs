//! The console verbosity: the `--quiet` startup option (docs/STARTUP_QUIET.md).
//!
//! Bazel 9.3's `--quiet` is a client option the client sends with each
//! request (blaze.cc:2041, `request.set_quiet`); the server's
//! UiEventHandler then drops every event kind except ERROR, FATAL, STDOUT
//! and STDERR (UiEventHandler.java:538-545) and never draws the progress
//! bar (:1140). So under `--quiet` the console keeps the errors (with a
//! failed action's output), what Bazel writes to its own stdout/stderr
//! streams (the `Target //x up-to-date:` result lines, the test summary,
//! `execute(quiet = False)` echoes, a `run` binary's own output) — and
//! loses the DEBUG (`print()`), INFO (`INFO: From …` with a successful
//! action's or test's output, `INFO: Running command line`, the summary
//! lines), WARNING and PROGRESS events and the FAIL lines of tests.
//!
//! The native side has no event bus: every informational line goes through
//! [`note`] (the [`crate::note!`] macro), the one verbosity check. Bytes
//! that are stored before they reach stderr — what planning printed, which
//! an analysis snapshot keeps and a hit replays (crate::snapshot::capture),
//! a repository speculation's deferred output — keep their notes as
//! *marked* segments ([`marked`]): every writer of such bytes to the console
//! passes them through [`Filter`], which strips the marks and, under
//! `--quiet`, the notes. So `--quiet` is console-only, like Bazel's (it
//! never restarts the server): it is no part of the snapshot key, a
//! snapshot saved by a quiet run replays every note to a loud one and vice
//! versa, and the loud output is byte-identical to what it was without the
//! marks.

use std::borrow::Cow;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

static QUIET: AtomicBool = AtomicBool::new(false);

/// Set the command's verbosity (the startup options' `--[no]quiet`).
pub fn set_quiet(quiet: bool) {
    QUIET.store(quiet, Ordering::SeqCst);
}

/// Whether `--quiet` is in effect: only errors and the command's own
/// output reach the console. Every console display (a progress area
/// included) honours it.
pub fn quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

/// The mark opening a note in stored stderr bytes: an APC escape sequence
/// (`ESC _ … ESC \`), which terminals do not display should one ever leak.
const START: &[u8] = b"\x1b_bazel-rs:note\x1b\\";
/// The mark closing a note.
const END: &[u8] = b"\x1b_bazel-rs:end\x1b\\";

/// `text` and its newline as a marked note (see the module documentation).
pub fn marked(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(START.len() + text.len() + 1 + END.len());
    bytes.extend_from_slice(START);
    bytes.extend_from_slice(text.as_bytes());
    bytes.push(b'\n');
    bytes.extend_from_slice(END);
    bytes
}

/// Print an informational line (`text` without its newline): Bazel's
/// INFO, WARNING, DEBUG and PROGRESS events, the native `bazel-rs: …`
/// notes. Dropped under `--quiet`; while a stderr capture records what
/// planning prints, written marked into it (its reader forwards it
/// filtered, a snapshot hit replays it filtered).
pub fn note(text: &str) {
    if crate::snapshot::capture::is_active() {
        let bytes = marked(text);
        let _ = std::io::stderr().lock().write_all(&bytes);
    } else if !quiet() {
        eprintln!("{text}");
    }
}

/// Write stderr bytes that may hold marked notes ([`marked`]): as they are
/// into an active capture (whose reader filters what it forwards), else
/// filtered.
pub fn emit(bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    let mut stderr = std::io::stderr().lock();
    if crate::snapshot::capture::is_active() {
        let _ = stderr.write_all(bytes);
    } else {
        let _ = stderr.write_all(&filtered(bytes));
    }
    let _ = stderr.flush();
}

/// `bytes` (whole, e.g. a snapshot's window stderr) as the console shows
/// them under the current verbosity.
pub fn filtered(bytes: &[u8]) -> Cow<'_, [u8]> {
    if !bytes.contains(&0x1b) {
        return Cow::Borrowed(bytes);
    }
    let mut filter = Filter::new(quiet());
    let mut out = Vec::with_capacity(bytes.len());
    filter.feed(bytes, &mut out);
    filter.finish(&mut out);
    Cow::Owned(out)
}

/// A streaming filter of stderr bytes holding marked notes: strips the
/// marks, drops the notes when `quiet`. Marks may be split across chunks.
#[derive(Debug)]
pub struct Filter {
    quiet: bool,
    in_note: bool,
    /// A chunk's tail that may be the start of a mark.
    held: Vec<u8>,
}

impl Filter {
    pub fn new(quiet: bool) -> Self {
        Self {
            quiet,
            in_note: false,
            held: Vec::new(),
        }
    }

    /// Filter `input` (the bytes after everything fed before) into `out`.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) {
        if self.held.is_empty() && !input.contains(&0x1b) {
            self.pass(input, out);
            return;
        }
        let mut data = std::mem::take(&mut self.held);
        data.extend_from_slice(input);
        let mut rest = &data[..];
        loop {
            let mark = if self.in_note { END } else { START };
            match find(rest, mark) {
                Some(at) => {
                    self.pass(&rest[..at], out);
                    rest = &rest[at + mark.len()..];
                    self.in_note = !self.in_note;
                }
                None => {
                    // Hold back a tail that may begin a mark (of either
                    // kind: a stray mark is passed on as it is).
                    let keep = partial_mark(rest);
                    self.pass(&rest[..rest.len() - keep], out);
                    self.held = rest[rest.len() - keep..].to_vec();
                    return;
                }
            }
        }
    }

    /// The end of the stream: what was held back goes out.
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        let held = std::mem::take(&mut self.held);
        self.pass(&held, out);
    }

    fn pass(&self, bytes: &[u8], out: &mut Vec<u8>) {
        if !(self.in_note && self.quiet) {
            out.extend_from_slice(bytes);
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The length of the longest tail of `bytes` that could begin a mark.
fn partial_mark(bytes: &[u8]) -> usize {
    let longest = START.len().max(END.len()) - 1;
    (1..=longest.min(bytes.len()))
        .rev()
        .find(|&len| {
            let tail = &bytes[bytes.len() - len..];
            START.starts_with(tail) || END.starts_with(tail)
        })
        .unwrap_or(0)
}

/// The `print()` handler of evaluations whose prints go straight to the
/// console (MODULE.bazel files, module extension implementations): DEBUG
/// events, [`note`]s.
pub struct NotePrints;

impl starlark::PrintHandler for NotePrints {
    fn println(&self, text: &str) -> starlark::Result<()> {
        note(&crate::debug_print::line(text));
        Ok(())
    }
}

/// Print an informational line under the verbosity check of
/// [`crate::ui::note`] (`format!` arguments, without the newline).
#[macro_export]
macro_rules! note {
    ($($arg:tt)*) => {
        $crate::ui::note(&::std::format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(quiet: bool, chunks: &[&[u8]]) -> Vec<u8> {
        let mut filter = Filter::new(quiet);
        let mut out = Vec::new();
        for chunk in chunks {
            filter.feed(chunk, &mut out);
        }
        filter.finish(&mut out);
        out
    }

    #[test]
    fn strips_marks_or_notes() {
        let mut bytes = b"ERROR: one\n".to_vec();
        bytes.extend(marked("DEBUG: two\nlines"));
        bytes.extend_from_slice(b"raw\n");
        bytes.extend(marked("WARNING: three"));
        assert_eq!(
            run(false, &[&bytes]),
            b"ERROR: one\nDEBUG: two\nlines\nraw\nWARNING: three\n"
        );
        assert_eq!(run(true, &[&bytes]), b"ERROR: one\nraw\n");
        // Every split point, marks cut anywhere.
        for at in 0..bytes.len() {
            let (a, b) = bytes.split_at(at);
            assert_eq!(run(true, &[a, b]), b"ERROR: one\nraw\n", "split at {at}");
            assert_eq!(
                run(false, &[a, b]),
                b"ERROR: one\nDEBUG: two\nlines\nraw\nWARNING: three\n",
                "split at {at}"
            );
        }
    }

    #[test]
    fn plain_bytes_pass() {
        assert_eq!(
            run(true, &[b"a\x1b[1mb\x1b", b"_x\n"]),
            b"a\x1b[1mb\x1b_x\n"
        );
        assert!(matches!(filtered(b"plain\n"), Cow::Borrowed(_)));
    }
}
