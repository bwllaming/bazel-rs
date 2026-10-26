//! Spawn supervision: Bazel's process-wrapper for every local spawn
//! (docs/PROCESS_WRAPPER.md).
//!
//! Genuine Bazel 9.3 starts every local spawn under a supervisor process that
//! outlives the spawn's main process: the process-wrapper for standalone and
//! processwrapper-sandbox spawns (LocalSpawnRunner.java:375-386,
//! StandaloneModule.java:89) and for repository_ctx/module_ctx `execute`
//! (StarlarkBaseExternalContext.java:2079-2085, BazelRepositoryModule.java:
//! 294-297), the linux-sandbox for the linux-sandbox strategy. The
//! process-wrapper makes itself a child subreaper before it forks the command
//! (process-wrapper-legacy.cc:53-62), so every descendant that leaves the
//! command's process group (a `setsid` daemon such as embedded PostgreSQL's
//! `pg_ctl start`, a double fork) is reparented to it; once the command has
//! exited it SIGKILLs the command's process group and then every remaining
//! child, again and again, until none is left (TerminateAndWaitForAll,
//! process-tools-linux.cc:24-55) and exits with the command's status. The
//! linux-sandbox ends a PID namespace instead (linux-sandbox-pid1.cc:811-814):
//! no process of the spawn survives it either way.
//!
//! This module is that supervisor, in both halves:
//!
//! - the executor half ([`Launch`], [`Spawned`]): the executor starts its own
//!   binary (/proc/self/exe) as `__process-wrapper` (standalone spawns,
//!   repository execute) or `__sandbox-worker` (crate::sandbox, which applies
//!   the sandbox and then supervises the same way) with one end of a Unix
//!   socket pair as the supervisor's stdin — the control channel;
//! - the supervisor half ([`main`], [`supervise`]), single-threaded in the
//!   re-executed binary: it becomes a child subreaper, starts the command in
//!   its own process group with an empty stdin, waits for it, then kills and
//!   reaps every remaining descendant and reports the command's exact wait
//!   status.
//!
//! The control channel carries, supervisor to executor, `S<pid>\n` once the
//! command runs, `E<errno>\n` when it cannot be started (the executor returns
//! the same `io::Error` a direct start would), and `X<wait status>\n` after the
//! command and every descendant are gone (the executor rebuilds the identical
//! `ExitStatus`, signal and core flag included — the supervisor cannot re-raise
//! SIGPIPE or SIGSEGV faithfully without unsafe code). Executor to supervisor:
//! `T` sends SIGTERM to the command's process group (a test timeout, like the
//! process-wrapper's SIGALRM handler, process-tools.cc:93-111), `K` SIGKILLs
//! the command and its group (the end of the grace period, a deadline), and
//! end-of-file — the executor exited (Ctrl-C, the memory watchdog's exit 33, a
//! crash) or dropped the spawn — is a `K` too, like linux-sandbox dying with
//! its parent (linux-sandbox.cc:30-33). Escaped descendants are killed after
//! the command in every case.
//!
//! Only the CLI binary can be re-executed as the supervisor: main calls
//! [`enable`]. The library's in-process users (unit tests, benches) start
//! standalone spawns directly with the former process-group discipline (the
//! group is SIGKILLed once its leader has exited, while the unreaped leader
//! still pins the group's ID; a process that left the group is not reached).

use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use rustix::process::{Pid, Signal, WaitId, WaitIdOptions};

/// The hidden argv[1] that dispatches main to [`main`].
pub const ARG: &str = "__process-wrapper";

/// The executor's own binary, re-executed as the supervisor.
const SELF_EXE: &str = "/proc/self/exe";

/// The errno reported for a start error that carries none (std's start
/// errors all carry one; a malformed command line would already have failed
/// the supervisor's own start).
const EINVAL: i32 = 22;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Marks this process as the CLI binary, which can re-execute itself as the
/// supervisor (main, before any spawn). Without it standalone spawns start
/// directly.
pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

/// Whether standalone spawns run under the process-wrapper ([`enable`]).
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// One line of the supervisor's reports.
#[derive(Debug, PartialEq, Eq)]
enum Report {
    /// The command runs with this PID (its own process group's leader).
    Started(u32),
    /// The command could not be started (the start's errno).
    Failed(i32),
    /// The command's raw wait status, sent once every descendant is gone.
    Exited(i32),
}

impl Report {
    fn encode(&self) -> String {
        match self {
            Report::Started(pid) => format!("S{pid}\n"),
            Report::Failed(errno) => format!("E{errno}\n"),
            Report::Exited(raw) => format!("X{raw}\n"),
        }
    }

    fn decode(line: &[u8]) -> Option<Report> {
        let (kind, value) = line.split_first()?;
        let value = std::str::from_utf8(value).ok()?;
        match kind {
            b'S' => value.parse().ok().map(Report::Started),
            b'E' => value.parse().ok().map(Report::Failed),
            b'X' => value.parse().ok().map(Report::Exited),
            _ => None,
        }
    }
}

/// One end of the control channel with its pending input.
struct Control {
    stream: UnixStream,
    pending: Vec<u8>,
}

impl Control {
    fn new(stream: UnixStream) -> Self {
        Self {
            stream,
            pending: Vec::new(),
        }
    }

    /// The next report line (without its newline); None at end-of-file or on
    /// a read error. Blocking: the executor reads a report only when one is
    /// due (the start report right after the start, the end report after the
    /// supervisor exited).
    fn line(&mut self) -> Option<Vec<u8>> {
        loop {
            if let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
                let line = self.pending[..end].to_vec();
                self.pending.drain(..=end);
                return Some(line);
            }
            let mut buffer = [0u8; 64];
            match self.stream.read(&mut buffer) {
                Ok(0) => return None,
                Ok(read) => self.pending.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
        }
    }

    /// The next report that decodes; None at end-of-file.
    fn report(&mut self) -> Option<Report> {
        self.line().and_then(|line| Report::decode(&line))
    }
}

// ---------------------------------------------------------------------------
// The executor half.

/// A spawn about to start: the supervisor's command line (process-wrapper or
/// sandbox worker), or the spawn's own when no supervisor is available. The
/// caller configures the working directory, environment and stdout/stderr
/// through [`Launch::command`]; stdin is the launch's (the control channel, or
/// empty).
pub struct Launch {
    command: Command,
    supervised: bool,
}

impl Launch {
    /// `program` with argv[0] `arg0` (the program itself when None) and
    /// `args`: under the process-wrapper when this process is the CLI binary
    /// ([`enabled`]), else started directly.
    pub fn new<S: AsRef<OsStr>>(
        program: impl AsRef<OsStr>,
        arg0: Option<&OsStr>,
        args: impl IntoIterator<Item = S>,
    ) -> Self {
        if enabled() {
            let mut command = Command::new(SELF_EXE);
            command.arg(ARG);
            if let Some(arg0) = arg0 {
                command.arg("--arg0").arg(arg0);
            }
            command.arg("--").arg(program.as_ref()).args(args);
            Self {
                command,
                supervised: true,
            }
        } else {
            let mut command = Command::new(program.as_ref());
            if let Some(arg0) = arg0 {
                command.arg0(arg0);
            }
            command.args(args);
            Self {
                command,
                supervised: false,
            }
        }
    }

    /// The sandbox worker with its argv (crate::sandbox::wrap): a supervisor
    /// itself.
    pub fn sandbox_worker(argv: Vec<String>) -> Self {
        let mut command = Command::new(SELF_EXE);
        command.args(argv);
        Self {
            command,
            supervised: true,
        }
    }

    /// `repository_ctx.execute` under the process-wrapper: the supervisor runs
    /// glibc execvp's search over `candidates` for `argv`
    /// (crate::repository_process::search). Only with [`enabled`].
    pub(crate) fn execvp(candidates: &[PathBuf], argv: &[Vec<u8>]) -> Self {
        let mut command = Command::new(SELF_EXE);
        command.arg(ARG).arg("--execvp");
        for candidate in candidates {
            command.arg("--candidate").arg(candidate);
        }
        command
            .arg("--")
            .args(argv.iter().map(|arg| OsStr::from_bytes(arg)));
        Self {
            command,
            supervised: true,
        }
    }

    /// The command to configure (working directory, environment, stdout and
    /// stderr; the supervisor passes them on to the spawn unchanged).
    pub fn command(&mut self) -> &mut Command {
        &mut self.command
    }

    /// Starts the spawn in its own process group. A supervised spawn returns
    /// once the supervisor has started the command (or failed to: the error is
    /// the one a direct start would return, e.g. ENOENT or ETXTBSY).
    pub fn start(&mut self) -> io::Result<Spawned> {
        // Its own process group: terminal signals for bazel-rs's group never
        // reach it, and the memory watchdog can address the spawn's group.
        self.command.process_group(0);
        if !self.supervised {
            self.command.stdin(Stdio::null());
            crate::signals::reset_in_child(
                &mut self.command,
                crate::signals::Defaults::ProcessWrapper,
            );
            return crate::spawn_lock::spawn(&mut self.command).map(Spawned::direct);
        }
        let (ours, theirs) = UnixStream::pair()?;
        self.command.stdin(Stdio::from(OwnedFd::from(theirs)));
        let started = crate::spawn_lock::spawn_supervisor(&mut self.command);
        // Drop this process's copy of the supervisor's end: end-of-file on
        // ours then means the supervisor is gone.
        self.command.stdin(Stdio::null());
        let mut child = started?;
        let mut control = Control::new(ours);
        match control.report() {
            Some(Report::Started(pid)) => {
                if crate::memory_limit::watchdog_armed() {
                    crate::memory_limit::register_live_group(pid);
                }
                Ok(Spawned {
                    child,
                    control: Some(control),
                    pid: Some(pid),
                    status: None,
                })
            }
            Some(Report::Failed(errno)) => {
                let _ = child.wait();
                Err(io::Error::from_raw_os_error(errno))
            }
            // The supervisor ended without starting the command (a sandbox
            // setup failure): its own exit status is the result.
            _ => Ok(Spawned {
                child,
                control: Some(control),
                pid: None,
                status: None,
            }),
        }
    }
}

/// A started spawn: its supervisor, or the command itself when started
/// directly.
pub struct Spawned {
    child: Child,
    /// The control channel (None: started directly).
    control: Option<Control>,
    /// The supervised command's PID, registered with the memory watchdog
    /// until the spawn ends.
    pid: Option<u32>,
    status: Option<ExitStatus>,
}

impl Spawned {
    /// A command started directly in its own process group.
    pub(crate) fn direct(child: Child) -> Self {
        Self {
            child,
            control: None,
            pid: None,
            status: None,
        }
    }

    /// The command's PID (None when the supervisor ended before starting it).
    pub fn id(&self) -> Option<u32> {
        match self.control {
            Some(_) => self.pid,
            None => Some(self.child.id()),
        }
    }

    /// The spawn's stdout pipe, when the command was configured with one.
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    /// The spawn's stderr pipe, when the command was configured with one.
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    /// SIGTERM to the command's process group (a test timeout; the
    /// process-wrapper's graceful kill, process-tools.cc:93-107).
    pub fn terminate(&mut self) {
        self.signal(b'T', Signal::TERM);
    }

    /// SIGKILL to the command and its process group (the end of the grace
    /// period, a deadline); what it leaves is killed once it is gone.
    pub fn kill(&mut self) {
        self.signal(b'K', Signal::KILL);
    }

    fn signal(&mut self, command: u8, signal: Signal) {
        if self.status.is_some() {
            return;
        }
        match &mut self.control {
            // A supervisor that is gone cannot be told anything (EPIPE).
            Some(control) => {
                let _ = control.stream.write_all(&[command]);
            }
            None => {
                let _ = crate::git_process::signal_group(self.child.id(), signal);
            }
        }
    }

    /// The command's exit status once the spawn has ended (for a supervised
    /// spawn: once every process of it is gone).
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        if self.control.is_none() {
            return self.reap_direct(false);
        }
        match self.child.try_wait()? {
            Some(own) => Ok(Some(self.finish(own))),
            None => Ok(None),
        }
    }

    /// Waits for the spawn to end; the command's exit status.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        if self.control.is_none() {
            return self
                .reap_direct(true)?
                .ok_or_else(|| io::Error::other("the spawn did not end"));
        }
        let own = self.child.wait()?;
        Ok(self.finish(own))
    }

    /// Waits for the spawn while collecting its piped stdout and stderr.
    pub fn wait_with_output(self) -> io::Result<Output> {
        let Spawned {
            child,
            control,
            pid,
            status: _,
        } = self;
        let mut output = child.wait_with_output()?;
        if let Some(mut control) = control {
            output.status = end_status(&mut control, pid, output.status);
        }
        Ok(output)
    }

    /// The supervised command's status from the end report (the supervisor's
    /// own status when it reported none: it ended before the command started,
    /// or was killed itself).
    fn finish(&mut self, own: ExitStatus) -> ExitStatus {
        let status = match &mut self.control {
            Some(control) => end_status(control, self.pid, own),
            None => own,
        };
        self.status = Some(status);
        status
    }

    /// A direct start: once the leader has exited — still unreaped, so its
    /// PID pins the group's ID — whatever is left of its process group is
    /// SIGKILLed, then the leader is reaped (the process-wrapper's group kill,
    /// process-wrapper-legacy.cc:150-154, without a subreaper).
    fn reap_direct(&mut self, block: bool) -> io::Result<Option<ExitStatus>> {
        let pid = Pid::from_child(&self.child);
        let mut options = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT;
        if !block {
            options |= WaitIdOptions::NOHANG;
        }
        loop {
            match rustix::process::waitid(WaitId::Pid(pid), options) {
                Ok(Some(_)) => break,
                Ok(None) => return Ok(None),
                Err(rustix::io::Errno::INTR) => {}
                // Not waitable: leave the group alone rather than risk a
                // reused ID.
                Err(_) => {
                    let status = if block {
                        Some(self.child.wait()?)
                    } else {
                        self.child.try_wait()?
                    };
                    self.status = status;
                    return Ok(status);
                }
            }
        }
        let _ = crate::git_process::kill_group(self.child.id());
        let status = self.child.wait()?;
        self.status = Some(status);
        Ok(Some(status))
    }
}

/// Reads the end report of a supervisor that has exited and ends the
/// command's watchdog registration.
fn end_status(control: &mut Control, pid: Option<u32>, own: ExitStatus) -> ExitStatus {
    let mut status = own;
    while let Some(report) = control.report() {
        if let Report::Exited(raw) = report {
            status = ExitStatus::from_raw(raw);
            break;
        }
    }
    if let Some(pid) = pid {
        crate::memory_limit::unregister_live_group(pid);
    }
    status
}

// ---------------------------------------------------------------------------
// The supervisor half (the re-executed binary; single-threaded).

/// The supervisor's end of the control channel: its stdin when that is a
/// socket (the executor passes one); None otherwise (a manual run), which
/// only loses the reports and commands.
fn control_from_stdin() -> Option<Control> {
    let fd = io::stdin().as_fd().try_clone_to_owned().ok()?;
    let stream = UnixStream::from(fd);
    stream.peer_addr().ok()?;
    stream.set_nonblocking(true).ok()?;
    Some(Control::new(stream))
}

fn send(control: &mut Option<Control>, report: &Report) {
    if let Some(control) = control {
        // Blocking for the write (a few bytes into an empty buffer); the
        // executor may already be gone (EPIPE; SIGPIPE is ignored).
        let _ = control.stream.set_nonblocking(false);
        let _ = control.stream.write_all(report.encode().as_bytes());
        let _ = control.stream.set_nonblocking(true);
    }
}

/// Makes this process a child subreaper before its first child exists, so
/// orphaned descendants of the command are reparented here
/// (process-wrapper-legacy.cc:53-62; EINVAL — no such prctl — only loses the
/// cleanup of escaped descendants, as there).
fn become_subreaper() -> bool {
    rustix::process::set_child_subreaper(Some(rustix::process::getpid())).is_ok()
}

/// Starts the command the supervisor runs: an empty stdin and its own process
/// group (the process-wrapper's child `setsid()`s; this one, like the former
/// direct starts, only leaves the group), stdout, stderr, working directory
/// and environment inherited.
fn start_command(command: &mut Command) -> io::Result<Child> {
    command.stdin(Stdio::null()).process_group(0);
    // Every signal at its default action (ClearSignalMask in the
    // process-wrapper's child, process-wrapper-legacy.cc:72).
    crate::signals::reset_in_child(command, crate::signals::Defaults::ProcessWrapper);
    crate::spawn_lock::spawn(command)
}

/// The sandbox worker's supervision of its action (crate::sandbox, after the
/// sandbox is applied): `command` starts and is supervised exactly like the
/// process-wrapper's; the result is the worker's exit code. A start error is
/// returned unreported: the worker reports it as a sandbox setup failure.
pub(crate) fn run_supervised(command: &mut Command) -> io::Result<i32> {
    let mut control = control_from_stdin();
    let subreaper = become_subreaper();
    let child = start_command(command)?;
    send(&mut control, &Report::Started(child.id()));
    Ok(supervise(child, control, subreaper))
}

/// The supervisor's options: `[--arg0 S] [--execvp [--candidate PATH]...] --
/// PROGRAM ARGS...`.
struct Options {
    arg0: Option<OsString>,
    execvp: bool,
    candidates: Vec<PathBuf>,
    command_line: Vec<OsString>,
}

fn parse(args: &[OsString]) -> Result<Options, String> {
    let mut options = Options {
        arg0: None,
        execvp: false,
        candidates: Vec::new(),
        command_line: Vec::new(),
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].to_str() {
            Some("--arg0") => {
                index += 1;
                options.arg0 = Some(args.get(index).ok_or("--arg0 requires a value")?.clone());
            }
            Some("--execvp") => options.execvp = true,
            Some("--candidate") => {
                index += 1;
                options.candidates.push(PathBuf::from(
                    args.get(index).ok_or("--candidate requires a value")?,
                ));
            }
            Some("--") => {
                options.command_line = args[index + 1..].to_vec();
                break;
            }
            _ => return Err(format!("unknown option {:?}", args[index])),
        }
        index += 1;
    }
    if options.command_line.is_empty() {
        return Err("no command after --".into());
    }
    Ok(options)
}

/// The `__process-wrapper` entry point (main dispatches it before anything
/// else): start the command, supervise it, exit with its status.
pub fn main(args: &[OsString]) -> i32 {
    let options = match parse(args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("process-wrapper: {error}");
            return 1;
        }
    };
    let mut control = control_from_stdin();
    let subreaper = become_subreaper();
    let command_line = &options.command_line;
    let started = if options.execvp {
        let first = command_line[0].as_bytes();
        crate::repository_process::search(&options.candidates, first, |program, shell| {
            // glibc execvp runs a file that fails with ENOEXEC through /bin/sh.
            let mut command = Command::new(if shell { Path::new("/bin/sh") } else { program });
            if shell {
                command.arg(program);
            } else {
                command.arg0(&command_line[0]);
            }
            command.args(&command_line[1..]);
            start_command(&mut command)
        })
    } else {
        let mut command = Command::new(&command_line[0]);
        if let Some(arg0) = &options.arg0 {
            command.arg0(arg0);
        }
        command.args(&command_line[1..]);
        start_command(&mut command).map_err(|error| error.raw_os_error().unwrap_or(EINVAL))
    };
    match started {
        Ok(child) => {
            send(&mut control, &Report::Started(child.id()));
            supervise(child, control, subreaper)
        }
        Err(errno) => {
            send(&mut control, &Report::Failed(errno));
            1
        }
    }
}

/// The supervisor's work once `child` runs (started after
/// [`become_subreaper`]): obey the executor's commands until the child exits,
/// kill its process group and every remaining descendant, report the child's
/// wait status and return the supervisor's own exit code (the child's code, or
/// 128 + its signal like linux-sandbox, linux-sandbox.cc:272-280).
fn supervise(mut child: Child, mut control: Option<Control>, subreaper: bool) -> i32 {
    let pid = Pid::from_child(&child);
    // Readable once the child has exited; without pidfds (Linux < 5.3) the
    // exit is polled.
    let pidfd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()).ok();
    // Reparented descendants are reaped as they exit while the child runs.
    let reaper = subreaper
        .then(|| {
            std::thread::Builder::new()
                .name("orphan-reaper".into())
                .spawn(move || reap_orphans_while_running(pid))
                .ok()
        })
        .flatten();
    let exited = loop {
        match rustix::process::waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        ) {
            Ok(Some(_)) => break true,
            Ok(None) | Err(rustix::io::Errno::INTR) => {}
            Err(_) => break false,
        }
        wait_for_event(pidfd.as_ref(), control.as_ref());
        if let Some(channel) = control.as_mut() {
            let (commands, closed) = read_commands(channel);
            for command in commands {
                match command {
                    b'T' => {
                        let _ = rustix::process::kill_process_group(pid, Signal::TERM);
                    }
                    b'K' => kill_child(pid),
                    _ => {}
                }
            }
            if closed {
                // The executor is gone or gave the spawn up.
                kill_child(pid);
                control = None;
            }
        }
    };
    // The reaper ends once it sees the exited child (left unreaped for this
    // thread); otherwise it is left to the process exit.
    if exited && let Some(reaper) = reaper {
        let _ = reaper.join();
    }
    // The child has exited and is not reaped yet: its PID still pins its
    // process group's ID, so only the group's own members are reached.
    let _ = rustix::process::kill_process_group(pid, Signal::KILL);
    let raw = loop {
        match child.wait() {
            Ok(status) => break Some(status.into_raw()),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break None,
        }
    };
    if subreaper {
        kill_remaining_children();
    }
    match raw {
        Some(raw) => {
            send(&mut control, &Report::Exited(raw));
            let status = ExitStatus::from_raw(raw);
            status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
        }
        None => 1,
    }
}

/// Reaps every other child of this subreaper as soon as it exits while
/// `command` runs — the command's descendants reparented here — like the
/// process-wrapper's wait() loop that discards such zombies until the
/// command's own status arrives (process-tools.cc:194-199) and linux-sandbox's
/// pid 1 (linux-sandbox-pid1.cc:591-610): a process that waits for a daemon's
/// end with kill(pid, 0) sees it gone. Returns once `command` has exited,
/// leaving it unreaped (its PID pins its process group's ID for the group
/// kill that follows). waitid without WNOWAIT is only ever applied to a
/// listed PID other than the command's.
fn reap_orphans_while_running(command: Pid) {
    let exited = |pid: Pid, options: WaitIdOptions| {
        loop {
            match rustix::process::waitid(WaitId::Pid(pid), WaitIdOptions::EXITED | options) {
                Ok(status) => return status.is_some(),
                Err(rustix::io::Errno::INTR) => {}
                // ECHILD: no such child (any more).
                Err(_) => return true,
            }
        }
    };
    loop {
        // Blocks until some child has exited, reaping nothing.
        match rustix::process::waitid(WaitId::All, WaitIdOptions::EXITED | WaitIdOptions::NOWAIT) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(_) => return,
        }
        if exited(command, WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT) {
            return;
        }
        let mut reaped = false;
        for child in children() {
            if let Some(child) = Pid::from_raw(child)
                && child != command
            {
                reaped |= exited(child, WaitIdOptions::NOHANG);
            }
        }
        if !reaped {
            // The exited child is not listed yet: look again shortly.
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

/// SIGKILL to the (unreaped) child itself — also when it left its group —
/// and to its process group.
fn kill_child(pid: Pid) {
    let _ = rustix::process::kill_process(pid, Signal::KILL);
    let _ = rustix::process::kill_process_group(pid, Signal::KILL);
}

/// Blocks until the child exits or the executor sends something.
fn wait_for_event(pidfd: Option<&OwnedFd>, control: Option<&Control>) {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let mut fds = Vec::with_capacity(2);
    if let Some(fd) = pidfd {
        fds.push(PollFd::new(fd, PollFlags::IN));
    }
    if let Some(control) = control {
        fds.push(PollFd::new(&control.stream, PollFlags::IN));
    }
    let tick = Timespec {
        tv_sec: 0,
        tv_nsec: 10_000_000,
    };
    let timeout = if pidfd.is_some() { None } else { Some(&tick) };
    // EINTR and other errors only end this wait early; the caller looks again.
    let _ = poll(&mut fds, timeout);
}

/// The executor's pending command bytes, and whether the channel is closed.
fn read_commands(control: &mut Control) -> (Vec<u8>, bool) {
    let mut commands = Vec::new();
    loop {
        let mut buffer = [0u8; 64];
        match control.stream.read(&mut buffer) {
            Ok(0) => return (commands, true),
            Ok(read) => commands.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return (commands, false),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return (commands, true),
        }
    }
}

/// TerminateAndWaitForAll's sweep (process-tools-linux.cc:24-55): every child
/// this subreaper still has — the command's descendants that outlived it and
/// were reparented here (a `setsid` daemon, a double fork) — is SIGKILLed and
/// reaped, round after round (a killed child's own children are reparented
/// here before it can be reaped), until none is left. A child that leads its
/// own process group takes the group with it: the unreaped child pins the
/// group's ID, and only its descendants can be members (a process can join a
/// group only inside its own session, and that session is the child's).
fn kill_remaining_children() {
    loop {
        // Reap whatever has already exited.
        loop {
            match rustix::process::waitid(
                WaitId::All,
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG,
            ) {
                Ok(Some(_)) | Err(rustix::io::Errno::INTR) => {}
                // Living children remain.
                Ok(None) => break,
                // ECHILD: none left.
                Err(_) => return,
            }
        }
        let children = children();
        if children.is_empty() {
            // /proc lists no child (a kernel without CONFIG_PROC_CHILDREN):
            // the rest is reparented on when this process exits, as in
            // genuine's fallback.
            return;
        }
        for child in children {
            let Some(child) = Pid::from_raw(child) else {
                continue;
            };
            if rustix::process::getpgid(Some(child)).ok() == Some(child) {
                let _ = rustix::process::kill_process_group(child, Signal::KILL);
            }
            let _ = rustix::process::kill_process(child, Signal::KILL);
        }
        // Wait for one of them; its children are reparented here first.
        match rustix::process::waitid(WaitId::All, WaitIdOptions::EXITED) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(_) => return,
        }
    }
}

/// This process's children, from every thread's /proc children list (a
/// reparented orphan becomes the child of one live thread of the subreaper).
fn children() -> Vec<i32> {
    let mut children = Vec::new();
    // snapshot-unrecorded: the supervisor process's own /proc entry, read
    // only in the re-executed supervisor (never during planning).
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return children;
    };
    for task in tasks.flatten() {
        // snapshot-unrecorded: as above, this process's own task list.
        if let Ok(text) = std::fs::read_to_string(task.path().join("children")) {
            children.extend(
                text.split_ascii_whitespace()
                    .filter_map(|pid| pid.parse::<i32>().ok()),
            );
        }
    }
    children
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn reports_round_trip_and_reject_garbage() {
        for report in [
            Report::Started(4_194_303),
            Report::Failed(26),
            Report::Exited(0),
            Report::Exited(9),
            Report::Exited(0x8b),
            Report::Exited(3 << 8),
        ] {
            let line = report.encode();
            assert!(line.ends_with('\n'));
            assert_eq!(
                Report::decode(line.trim_end().as_bytes()),
                Some(report),
                "{line}"
            );
        }
        for garbage in [&b""[..], b"S", b"Sx", b"Q1", b"X-", b"E 2"] {
            assert_eq!(Report::decode(garbage), None, "{garbage:?}");
        }
    }

    #[test]
    fn control_lines_split_coalesced_reports() {
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        ours.write_all(b"S12\nX256\npartial").unwrap();
        drop(ours);
        let mut control = Control::new(theirs);
        assert_eq!(control.report(), Some(Report::Started(12)));
        assert_eq!(control.report(), Some(Report::Exited(256)));
        // An unterminated line at end-of-file is no report.
        assert_eq!(control.report(), None);
    }

    #[test]
    fn options_parse_like_the_executor_writes_them() {
        let args: Vec<OsString> = [
            "--arg0",
            "tool",
            "--execvp",
            "--candidate",
            "/a/x",
            "--candidate",
            "/b/x",
            "--",
            "x",
            "--arg0",
            "--",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        let options = parse(&args).unwrap();
        assert_eq!(options.arg0.as_deref(), Some(OsStr::new("tool")));
        assert!(options.execvp);
        assert_eq!(
            options.candidates,
            [PathBuf::from("/a/x"), PathBuf::from("/b/x")]
        );
        // Everything after the first `--` is the command line, verbatim.
        assert_eq!(options.command_line, ["x", "--arg0", "--"]);
        assert!(parse(&[OsString::from("--")]).is_err());
        assert!(parse(&[OsString::from("--bogus")]).is_err());
    }

    #[test]
    fn direct_starts_keep_the_group_discipline() {
        // The library's tests never call enable(): starts are direct.
        assert!(!enabled());
        let temp = tempfile::tempdir_in("/tmp/opencode").unwrap();
        let marker = temp.path().join("grandchild");
        let mut launch = Launch::new(
            "/bin/sh",
            None,
            [
                "-c",
                "sleep 30 & echo $! > \"$1\"; exit 3",
                "sh",
                marker.to_str().unwrap(),
            ],
        );
        let mut spawned = launch.start().unwrap();
        let status = spawned.wait().unwrap();
        assert_eq!(status.code(), Some(3));
        // The background child shared the group and was killed with it (a
        // zombie until init or a subreaper reaps it).
        let grandchild: i32 = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let gone = || match std::fs::read_to_string(format!("/proc/{grandchild}/stat")) {
            Err(_) => true,
            Ok(stat) => stat
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !gone() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(gone(), "grandchild {grandchild} survived the group kill");
        // Terminate and kill reach the group of a running direct start.
        let mut launch = Launch::new("/bin/sh", None, ["-c", "sleep 30"]);
        let mut spawned = launch.start().unwrap();
        assert!(spawned.try_wait().unwrap().is_none());
        spawned.terminate();
        let status = spawned.wait().unwrap();
        assert_eq!(status.signal(), Some(15));
        // A finished spawn ignores further signals and keeps its status.
        spawned.kill();
        assert_eq!(spawned.try_wait().unwrap().unwrap().signal(), Some(15));
        // A start error is the direct start's own.
        let error = Launch::new("/nonexistent/n59-tool", None, Vec::<&str>::new())
            .start()
            .err()
            .unwrap();
        assert_eq!(error.raw_os_error(), Some(2));
    }
}
