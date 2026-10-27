//! The thread that owns the terminal while the progress display is on
//! (crate::progress): it forwards what reaches file descriptors 2 (and 1)
//! through the pipe [`start`] put there, keeps the status area below it,
//! and ends with [`finish`] / [`abandon`].

use super::Config;
use super::render::{self, Line, RateLimit};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Whether `INFO:` leads are colored (the display's `--color`).
static COLOR: AtomicBool = AtomicBool::new(false);

pub(super) fn color() -> bool {
    COLOR.load(Ordering::Relaxed)
}

/// The terminal's descriptor and the status area's height, for the signal
/// handler (which may only write).
static TERMINAL_FD: AtomicI32 = AtomicI32::new(-1);
static DRAWN: AtomicUsize = AtomicUsize::new(0);

/// A partial line is held back for its newline at most this long (or
/// [`PENDING_LIMIT`] bytes); then it is written as it is, and the status
/// area stays hidden until a line ends.
const HOLD: Duration = Duration::from_millis(500);
const PENDING_LIMIT: usize = 64 << 10;
/// What [`finish`] waits at most for the thread to forward what is left.
const FINISH_BOUND: Duration = Duration::from_secs(2);
/// What the thread drains at most after [`finish`] restored the
/// descriptors (a writer that never stops is forwarded directly after it).
const DRAIN_BOUND: Duration = Duration::from_millis(300);

#[derive(Default)]
struct Done {
    finished: Mutex<bool>,
    signal: Condvar,
}

impl Done {
    fn set(&self) {
        *self.finished.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.signal.notify_all();
    }

    fn wait(&self, bound: Duration) {
        let finished = self.finished.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = self
            .signal
            .wait_timeout_while(finished, bound, |finished| !*finished);
    }
}

/// The running display.
struct Session {
    terminal: Arc<File>,
    /// The original stdout, when stdout was redirected too.
    stdout: Option<OwnedFd>,
    control: io::PipeWriter,
    done: Arc<Done>,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);

/// The exit code [`finish`] hands the thread (for the summary).
static EXIT_CODE: AtomicI32 = AtomicI32::new(0);

fn session() -> std::sync::MutexGuard<'static, Option<Session>> {
    SESSION.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether stdout is the terminal stderr is (then it is redirected too, so
/// what the command prints there keeps its place relative to the status
/// area and to stderr).
fn stdout_is_the_terminal() -> bool {
    let (Ok(out), Ok(err)) = (
        rustix::fs::fstat(io::stdout().as_fd()),
        rustix::fs::fstat(io::stderr().as_fd()),
    ) else {
        return false;
    };
    rustix::termios::isatty(io::stdout().as_fd())
        && out.st_rdev == err.st_rdev
        && out.st_dev == err.st_dev
        && out.st_ino == err.st_ino
}

/// Start the display: redirect file descriptor 2 (and 1, when it is the same
/// terminal) into a pipe whose reader thread owns the terminal.
pub fn start(config: Config) -> io::Result<()> {
    let mut slot = session();
    if slot.is_some() {
        return Ok(());
    }
    let terminal = Arc::new(File::from(io::stderr().as_fd().try_clone_to_owned()?));
    let (reader, writer) = io::pipe()?;
    let (control_reader, control) = io::pipe()?;
    let stdout = if stdout_is_the_terminal() {
        Some(io::stdout().as_fd().try_clone_to_owned()?)
    } else {
        None
    };
    let done = Arc::new(Done::default());
    COLOR.store(config.color, Ordering::Relaxed);
    let mut painter = Painter::new(terminal.clone(), config);
    painter.sampler = Some(|shown| render::lines(&super::sample(), shown));
    let thread_done = done.clone();
    std::thread::Builder::new()
        .name("progress".to_owned())
        .spawn(move || run(painter, reader, control_reader, thread_done))?;
    let _ = io::stdout().flush();
    // On failure the writer drops here: the reader sees the end of the pipe
    // (and the control pipe's end when `control` drops).
    if stdout.is_some() {
        rustix::stdio::dup2_stdout(&writer).map_err(io::Error::from)?;
    }
    if let Err(error) = rustix::stdio::dup2_stderr(&writer) {
        if let Some(original) = &stdout {
            let _ = rustix::stdio::dup2_stdout(original);
        }
        return Err(error.into());
    }
    drop(writer);
    TERMINAL_FD.store(terminal.as_raw_fd(), Ordering::Relaxed);
    signals::install();
    *slot = Some(Session {
        terminal,
        stdout,
        control,
        done,
    });
    super::ACTIVE.store(true, Ordering::Relaxed);
    Ok(())
}

/// End the display before the process exits with `exit_code` (or before
/// `run` executes its target): restore the descriptors, let the thread
/// forward what is still in the pipe, erase the status area and print the
/// interactive summary. A no-op when the display is off.
pub fn finish(exit_code: i32) {
    end(Some(exit_code));
}

/// [`finish`] without the summary (the memory watchdog's exit, a panic).
pub fn abandon() {
    end(None);
}

fn end(exit_code: Option<i32>) {
    let Some(session) = session().take() else {
        if let Some(code) = exit_code {
            super::plain_summary(code);
        }
        return;
    };
    // The display prints its own summary (below), once.
    super::take_summary();
    let _ = io::stdout().flush();
    let _ = rustix::stdio::dup2_stderr(&*session.terminal);
    if let Some(original) = &session.stdout {
        let _ = rustix::stdio::dup2_stdout(original);
    }
    if let Some(code) = exit_code {
        EXIT_CODE.store(code, Ordering::Relaxed);
    }
    let command = [if exit_code.is_some() { b'f' } else { b'a' }];
    if (&session.control).write_all(&command).is_ok() {
        session.done.wait(FINISH_BOUND);
    }
    signals::restore();
    super::ACTIVE.store(false, Ordering::Relaxed);
}

/// The terminal's width in columns (TIOCGWINSZ), else `fallback`.
fn width(terminal: &File, fallback: usize) -> usize {
    rustix::termios::tcgetwinsize(terminal)
        .ok()
        .map(|size| usize::from(size.ws_col))
        .filter(|columns| *columns > 0)
        .unwrap_or(fallback)
        .max(10)
}

/// The escape sequence that erases a status area of `drawn` lines whose
/// last line holds the cursor: back to its first line's start, then clear
/// to the end of the screen.
fn erase(drawn: usize, frame: &mut Vec<u8>) {
    if drawn == 0 {
        return;
    }
    frame.push(b'\r');
    if drawn > 1 {
        frame.extend_from_slice(format!("\x1b[{}A", drawn - 1).as_bytes());
    }
    frame.extend_from_slice(b"\x1b[J");
}

/// The status area drawn over one of `drawn` lines (the cursor at the end of
/// its last line), line by line in place — no blank frame in between — and
/// what is left below cleared.
fn redraw(drawn: usize, lines: &[String], frame: &mut Vec<u8>) {
    if drawn > 0 {
        frame.push(b'\r');
        if drawn > 1 {
            frame.extend_from_slice(format!("\x1b[{}A", drawn - 1).as_bytes());
        }
    }
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            frame.push(b'\n');
        }
        frame.extend_from_slice(line.as_bytes());
        frame.extend_from_slice(b"\x1b[K");
    }
    if drawn > 0 || !lines.is_empty() {
        frame.extend_from_slice(b"\x1b[J");
    }
}

/// The terminal side: what is on the screen below the forwarded output.
struct Painter {
    terminal: Arc<File>,
    config: Config,
    /// Lines of the status area on the screen (0: none).
    drawn: usize,
    /// The last forwarded bytes did not end a line: the cursor is inside
    /// it, so no status area is drawn until a line ends.
    mid_line: bool,
    /// A partial line held back for its newline.
    pending: Vec<u8>,
    pending_since: Option<Instant>,
    /// The last sampled status lines.
    status: Vec<Line>,
    /// Samples the status lines (None in unit tests: they set them).
    sampler: Option<fn(usize) -> Vec<Line>>,
    /// The last drawn rendering (to skip identical frames).
    rendered: Vec<String>,
    title: String,
    /// Writing to the terminal failed: forward best-effort, draw nothing.
    broken: bool,
}

impl Painter {
    fn new(terminal: Arc<File>, config: Config) -> Self {
        Self {
            terminal,
            config,
            drawn: 0,
            mid_line: false,
            pending: Vec::new(),
            pending_since: None,
            status: Vec::new(),
            sampler: None,
            rendered: Vec::new(),
            title: String::new(),
            broken: false,
        }
    }

    fn write(&mut self, frame: &[u8]) {
        if frame.is_empty() {
            return;
        }
        if (&*self.terminal).write_all(frame).is_err() {
            self.broken = true;
        }
        DRAWN.store(self.drawn, Ordering::Relaxed);
    }

    /// The current status lines rendered for the terminal's width.
    fn render(&self) -> Vec<String> {
        let width = width(&self.terminal, self.config.columns).saturating_sub(1);
        self.status
            .iter()
            .map(|line| line.render(width, self.config.color))
            .collect()
    }

    /// Bytes the process wrote: whole lines go out at once (the status area
    /// erased before and drawn again after them), a partial line waits.
    fn output(&mut self, bytes: &[u8], now: Instant) {
        self.pending.extend_from_slice(bytes);
        let end = match self.pending.iter().rposition(|byte| *byte == b'\n') {
            Some(at) => at + 1,
            None if self.pending.len() > PENDING_LIMIT => self.pending.len(),
            None => {
                self.pending_since.get_or_insert(now);
                return;
            }
        };
        let out: Vec<u8> = self.pending.drain(..end).collect();
        self.pending_since = (!self.pending.is_empty()).then_some(now);
        self.forward(&out);
    }

    /// Write a partial line held back too long as it is.
    fn flush_stale(&mut self, now: Instant) {
        if self
            .pending_since
            .is_some_and(|since| now.duration_since(since) >= HOLD)
        {
            let out = std::mem::take(&mut self.pending);
            self.pending_since = None;
            self.forward(&out);
        }
    }

    fn forward(&mut self, out: &[u8]) {
        if out.is_empty() {
            return;
        }
        let mut frame = Vec::with_capacity(out.len() + 256);
        erase(self.drawn, &mut frame);
        self.drawn = 0;
        frame.extend_from_slice(out);
        self.mid_line = out.last() != Some(&b'\n');
        if !self.mid_line && !self.broken {
            // Sampled afresh: the output may come with a new phase (the
            // `INFO: Analyzed` line), an action may have ended.
            self.sample();
            let lines = self.render();
            redraw(0, &lines, &mut frame);
            self.drawn = lines.len();
            self.rendered = lines;
        }
        self.write(&frame);
    }

    /// Sample the status lines now.
    fn sample(&mut self) {
        if let Some(sampler) = self.sampler {
            self.status = sampler(self.config.actions_shown);
        }
    }

    /// A rate-limited refresh with freshly sampled lines.
    fn refresh_live(&mut self) {
        self.sample();
        let status = std::mem::take(&mut self.status);
        self.refresh(status);
    }

    /// A refresh showing `status`.
    fn refresh(&mut self, status: Vec<Line>) {
        self.status = status;
        if self.mid_line || self.broken {
            return;
        }
        let lines = self.render();
        let mut frame = Vec::new();
        if self.config.title {
            let title = render::title(&self.status);
            if title != self.title {
                frame.extend_from_slice(format!("\x1b]0;{title}\x07").as_bytes());
                self.title = title;
            }
        }
        if lines != self.rendered || self.drawn != lines.len() {
            redraw(self.drawn, &lines, &mut frame);
            self.drawn = lines.len();
            self.rendered = lines;
        }
        self.write(&frame);
    }

    /// The end: the held-back partial line, the status area erased, the
    /// summary (when `summary`), the title reset.
    fn close(&mut self, summary: Option<i32>) {
        let out = std::mem::take(&mut self.pending);
        self.pending_since = None;
        let mut frame = Vec::new();
        erase(self.drawn, &mut frame);
        self.drawn = 0;
        frame.extend_from_slice(&out);
        if !out.is_empty() {
            self.mid_line = out.last() != Some(&b'\n');
        }
        if let Some(exit_code) = summary {
            if self.mid_line {
                frame.push(b'\n');
                self.mid_line = false;
            }
            let counters = &super::COUNTERS;
            for line in render::summary(
                super::start_instant().elapsed(),
                exit_code,
                &render::Totals::current(),
                counters.tests_failed.load(Ordering::Relaxed),
                self.config.color,
            ) {
                frame.extend_from_slice(line.as_bytes());
                frame.push(b'\n');
            }
        }
        if self.config.title && !self.title.is_empty() {
            frame.extend_from_slice(b"\x1b]0;\x07");
        }
        self.write(&frame);
    }
}

/// Wait at most `timeout` for `data` (when still open) or `control` to be
/// readable: (data ready, control ready).
fn wait(
    data: Option<&io::PipeReader>,
    control: &io::PipeReader,
    timeout: Duration,
) -> (bool, bool) {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let timeout = Timespec {
        tv_sec: timeout.as_secs() as i64,
        tv_nsec: i64::from(timeout.subsec_nanos()),
    };
    let ready = PollFlags::IN | PollFlags::HUP | PollFlags::ERR;
    let mut fds = Vec::with_capacity(2);
    fds.push(PollFd::new(control, PollFlags::IN));
    if let Some(data) = data {
        fds.push(PollFd::new(data, PollFlags::IN));
    }
    if poll(&mut fds, Some(&timeout)).is_err() {
        return (false, false);
    }
    let control_ready = fds[0].revents().intersects(ready);
    let data_ready = fds.get(1).is_some_and(|fd| fd.revents().intersects(ready));
    (data_ready, control_ready)
}

/// Read one chunk: None at the end of the pipe (or on an error).
fn read_chunk<'a>(data: &mut io::PipeReader, buffer: &'a mut [u8]) -> Option<&'a [u8]> {
    loop {
        match data.read(buffer) {
            Ok(0) => return None,
            Ok(count) => return Some(&buffer[..count]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

/// The thread: forward, refresh at the rate limit, and end on the control
/// byte (`f`: with the summary, `a`: without).
fn run(mut painter: Painter, data: io::PipeReader, mut control: io::PipeReader, done: Arc<Done>) {
    let mut data = Some(data);
    let mut buffer = vec![0u8; 64 << 10];
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut limit = RateLimit::new(painter.config.rate, Instant::now());
        loop {
            let now = Instant::now();
            let mut timeout = limit.remaining(now);
            if let Some(since) = painter.pending_since {
                timeout = timeout.min((since + HOLD).saturating_duration_since(now));
            }
            let (data_ready, control_ready) = wait(data.as_ref(), &control, timeout);
            if control_ready {
                let mut command = [0u8; 1];
                let summary = matches!(control.read(&mut command), Ok(1) if command[0] == b'f');
                // Everything written before the descriptors were restored
                // is in the pipe: forward it before the summary.
                let started = Instant::now();
                while let Some(reader) = data.as_mut() {
                    if started.elapsed() >= DRAIN_BOUND {
                        break;
                    }
                    let (ready, _) = wait(Some(reader), &control, Duration::ZERO);
                    if !ready {
                        break;
                    }
                    match read_chunk(reader, &mut buffer) {
                        Some(chunk) => painter.output(chunk, Instant::now()),
                        None => data = None,
                    }
                }
                painter.close(summary.then(|| EXIT_CODE.load(Ordering::Relaxed)));
                return;
            }
            if data_ready && let Some(reader) = data.as_mut() {
                match read_chunk(reader, &mut buffer) {
                    Some(chunk) => painter.output(chunk, Instant::now()),
                    None => data = None,
                }
            }
            let now = Instant::now();
            painter.flush_stale(now);
            if limit.due(now) {
                painter.refresh_live();
            }
        }
    }));
    DRAWN.store(0, Ordering::Relaxed);
    done.set();
    if outcome.is_err() {
        // Never leave the pipe without a reader (writers would fail): the
        // bytes go through unchanged from now on.
        let _ = (&*painter.terminal).write_all(b"\r\x1b[J");
    }
    // A process that still holds the pipe (a lingering child) writes on
    // through it: forward as it comes, until its end.
    if let Some(mut reader) = data {
        while let Some(chunk) = read_chunk(&mut reader, &mut buffer) {
            let _ = (&*painter.terminal).write_all(chunk);
        }
    }
}

/// SIGINT, SIGTERM and SIGHUP erase the status area, then end the process:
/// SIGINT with exit code 8 (crate::signals::exit_on_interrupt), the others
/// by the same signal (its default action).
#[allow(unsafe_code)]
mod signals {
    use super::{DRAWN, TERMINAL_FD};
    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    const SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

    /// The dispositions replaced by [`install`].
    static PREVIOUS: Mutex<Vec<(libc::c_int, libc::sigaction)>> = Mutex::new(Vec::new());

    extern "C" fn handler(signal: libc::c_int) {
        let fd = TERMINAL_FD.load(Ordering::Relaxed);
        let drawn = DRAWN.load(Ordering::Relaxed);
        if fd >= 0 && drawn > 0 {
            // "\r\x1b[<n>A\x1b[J" without allocating (async-signal-safe).
            let mut bytes = [0u8; 32];
            let mut at = 0;
            let mut push = |byte: u8| {
                bytes[at] = byte;
                at += 1;
            };
            push(b'\r');
            if drawn > 1 {
                push(0x1b);
                push(b'[');
                let up = (drawn - 1).min(9999);
                let mut digits = [0u8; 4];
                let mut count = 0;
                let mut value = up;
                loop {
                    digits[count] = b'0' + (value % 10) as u8;
                    count += 1;
                    value /= 10;
                    if value == 0 {
                        break;
                    }
                }
                for index in (0..count).rev() {
                    push(digits[index]);
                }
                push(b'A');
            }
            push(0x1b);
            push(b'[');
            push(b'J');
            // SAFETY: write(2) on a descriptor this process owns, from a
            // stack buffer; async-signal-safe.
            unsafe {
                libc::write(fd, bytes.as_ptr().cast(), at);
            }
        }
        // An interrupt exits 8 (crate::signals::exit_on_interrupt).
        if signal == libc::SIGINT {
            // SAFETY: _exit(2) is async-signal-safe.
            unsafe { libc::_exit(crate::signals::INTERRUPTED_EXIT_CODE) }
        }
        // SAFETY: SA_RESETHAND restored the default action on entry; the
        // re-raised signal is delivered when the handler returns.
        unsafe {
            libc::raise(signal);
        }
    }

    pub(super) fn install() {
        let mut previous = PREVIOUS.lock().unwrap_or_else(|e| e.into_inner());
        if !previous.is_empty() {
            return;
        }
        for signal in SIGNALS {
            // SAFETY: a zeroed sigaction is valid; the handler only calls
            // async-signal-safe functions.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = handler as *const () as libc::sighandler_t;
                action.sa_flags = libc::SA_RESETHAND;
                libc::sigemptyset(&mut action.sa_mask);
                let mut old: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(signal, &action, &mut old) != 0 {
                    continue;
                }
                if old.sa_sigaction == libc::SIG_IGN {
                    // An ignored signal (nohup) stays ignored.
                    libc::sigaction(signal, &old, std::ptr::null_mut());
                    continue;
                }
                previous.push((signal, old));
            }
        }
    }

    pub(super) fn restore() {
        let mut previous = PREVIOUS.lock().unwrap_or_else(|e| e.into_inner());
        for (signal, old) in previous.drain(..) {
            // SAFETY: re-installing the disposition sigaction returned.
            unsafe {
                libc::sigaction(signal, &old, std::ptr::null_mut());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::render::Tone;

    fn painter() -> (Painter, tempfile::NamedTempFile) {
        let file = tempfile::NamedTempFile::new().unwrap();
        let terminal = Arc::new(file.reopen().unwrap());
        let config = Config {
            color: false,
            rate: Duration::from_millis(100),
            actions_shown: 3,
            title: false,
            columns: 40,
        };
        (Painter::new(terminal, config), file)
    }

    fn written(file: &tempfile::NamedTempFile) -> String {
        String::from_utf8(std::fs::read(file.path()).unwrap()).unwrap()
    }

    fn status(texts: &[&str]) -> Vec<Line> {
        texts
            .iter()
            .map(|text| Line {
                lead: Some((Tone::Ok, "S:".to_owned())),
                text: format!(" {text}"),
            })
            .collect()
    }

    #[test]
    fn frames() {
        let mut frame = Vec::new();
        erase(0, &mut frame);
        assert!(frame.is_empty());
        erase(1, &mut frame);
        assert_eq!(frame, b"\r\x1b[J");
        frame.clear();
        erase(3, &mut frame);
        assert_eq!(frame, b"\r\x1b[2A\x1b[J");
        frame.clear();
        redraw(2, &["a".to_owned()], &mut frame);
        assert_eq!(frame, b"\r\x1b[1Aa\x1b[K\x1b[J");
        frame.clear();
        redraw(0, &["a".to_owned(), "b".to_owned()], &mut frame);
        assert_eq!(frame, b"a\x1b[K\nb\x1b[K\x1b[J");
        frame.clear();
        redraw(0, &[], &mut frame);
        assert!(frame.is_empty());
    }

    #[test]
    fn output_interleaves_with_the_status_area() {
        let (mut painter, file) = painter();
        let now = Instant::now();
        painter.refresh(status(&["one", "two"]));
        assert_eq!(written(&file), "S: one\x1b[K\nS: two\x1b[K\x1b[J");
        // An identical refresh writes nothing.
        painter.refresh(status(&["one", "two"]));
        assert_eq!(
            written(&file).len(),
            "S: one\x1b[K\nS: two\x1b[K\x1b[J".len()
        );
        // A partial line waits for its newline...
        painter.output(b"hello", now);
        assert_eq!(written(&file), "S: one\x1b[K\nS: two\x1b[K\x1b[J");
        // ...and goes out with it: erase, the line, the area again.
        painter.output(b" world\npart", now);
        assert!(
            written(&file)
                .ends_with("\r\x1b[1A\x1b[Jhello world\nS: one\x1b[K\nS: two\x1b[K\x1b[J")
        );
        assert_eq!(painter.drawn, 2);
        // A partial line held too long goes out as it is; the area hides.
        painter.flush_stale(now + HOLD);
        assert!(written(&file).ends_with("\r\x1b[1A\x1b[Jpart"));
        assert_eq!(painter.drawn, 0);
        assert!(painter.mid_line);
        painter.refresh(status(&["three"]));
        assert!(written(&file).ends_with("\r\x1b[1A\x1b[Jpart"));
        painter.output(b"!\n", now);
        assert!(written(&file).ends_with("part!\nS: three\x1b[K\x1b[J"));
        // The end: erased, then the summary.
        painter.close(Some(0));
        let text = written(&file);
        let tail = &text[text.rfind("\x1b[K\x1b[J").unwrap() + 6..];
        assert!(tail.starts_with("\r\x1b[J"), "{tail:?}");
        assert!(tail.contains("INFO: Elapsed time: "), "{tail:?}");
        assert!(tail.ends_with(" total actions\n"), "{tail:?}");
    }

    #[test]
    fn status_lines_fit_the_width() {
        let (mut painter, file) = painter();
        painter.refresh(status(&[&"x".repeat(100)]));
        let text = written(&file);
        let line = text.strip_suffix("\x1b[K\x1b[J").unwrap();
        // 40 columns (the fallback of a file), one left free.
        assert_eq!(line.chars().count(), 39);
        assert!(line.ends_with("..."));
    }

    #[test]
    fn close_without_summary_only_erases() {
        let (mut painter, file) = painter();
        painter.refresh(status(&["a", "b", "c"]));
        painter.output(b"tail", Instant::now());
        painter.close(None);
        assert!(written(&file).ends_with("\r\x1b[2A\x1b[Jtail"));
    }
}
