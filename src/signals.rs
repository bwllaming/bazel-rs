//! The signal dispositions a started process begins with.
//!
//! exec(2) resets a HANDLED signal to its default action but keeps an
//! IGNORED one ignored, so every signal this process inherited as ignored —
//! SIGINT and SIGQUIT in a background job of a non-interactive shell, SIGHUP
//! under nohup — would stay ignored in every child: a spawn's `kill -INT $$`
//! then does nothing. std resets only SIGPIPE (which its runtime ignores) and
//! the signal mask. Genuine Bazel 9.3 starts its processes with
//! (tests/fixtures/signal_dispositions: capture.py, reference_receipt.json):
//!
//! - [`Defaults::ProcessWrapper`]: every signal at its default action, for
//!   everything started through the process-wrapper or the linux-sandbox —
//!   actions, tests, repository_ctx/module_ctx `execute` (ClearSignalMask,
//!   process-tools.cc:151-173, called in the child by
//!   process-wrapper-legacy.cc:72 and linux-sandbox-pid1.cc:567). Native
//!   stand-ins for Starlark `execute` calls (crate::git_process for
//!   git_repository) belong here too.
//! - [`Defaults::Client`]: the client's handlers for SIGINT, SIGTERM,
//!   SIGPIPE, SIGQUIT and SIGWINCH (SignalHandler::Install,
//!   blaze_util_posix.cc:204-219) become default actions when it execs a
//!   `bazel run` command (ExecuteRunRequest); SIGHUP stays as inherited, so a
//!   nohup'd `bazel run` still runs its target immune to hangups.
//! - [`Defaults::Jvm`]: the server's own direct starts (the
//!   --workspace_status_command, credential helpers) inherit what the JVM
//!   inherited, except the signals HotSpot handles itself: SIGQUIT (thread
//!   dumps) and SIGPIPE. SIGINT, SIGTERM and SIGHUP stay ignored.
//!
//! **Held, not ignored** ([`hold_ignored`]): a `pre_exec` hook forces std's
//! fork path instead of posix_spawn, and forking a large many-threaded
//! process for every start is slow (the library suite started from a
//! background job: 18 s → 108–144 s). So the CLI, first thing (and any other
//! process at its first start), turns every signal it inherited ignored into
//! a *held* one: blocked in every thread (the mask is inherited by each
//! thread started later, so the signal is never delivered — bazel-rs itself
//! keeps ignoring it) with a handler that does nothing. exec(2) resets a
//! handled signal to its default action and std clears the signal mask of
//! every process it starts (posix_spawn's `POSIX_SPAWN_SETSIGMASK`, or in
//! the forked child), so a start that wants the defaults
//! ([`Defaults::ProcessWrapper`], the spawn supervisors) needs no hook at
//! all and keeps the posix_spawn fast path. Only a start that must keep a
//! held signal ignored gets a hook setting it back to SIG_IGN in the child
//! (fork): [`Defaults::Client`] and [`Defaults::Jvm`] for the signals they
//! keep, and every start through crate::spawn_lock::spawn without defaults
//! ([`inherit_in_child`]: it inherits the dispositions as they were). A
//! signal still ignored here (one the hold could not take) is reset in the
//! child by a hook as before. The progress display's handlers
//! (crate::progress) replace a held signal's no-op handler while they are
//! installed; blocked, they never run for it either.
//!
//! The other handwritten signal code is the progress display's handler
//! (crate::progress::terminal::signals), which needs that module's state.

#![allow(unsafe_code)]

use std::os::unix::process::CommandExt;
use std::process::Command;

/// Which inherited-ignored signals a started process gets back at their
/// default action (module documentation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Defaults {
    /// Every signal (the process-wrapper's ClearSignalMask).
    ProcessWrapper,
    /// SIGINT, SIGTERM, SIGQUIT and SIGWINCH (the client's exec of `run`).
    Client,
    /// SIGQUIT (a direct start of the JVM server).
    Jvm,
}

/// The highest signal number (SIGRTMAX on Linux; NSIG - 1).
const LAST_SIGNAL: libc::c_int = 64;

impl Defaults {
    fn covers(self, signal: libc::c_int) -> bool {
        match self {
            Defaults::ProcessWrapper => true,
            Defaults::Client => matches!(
                signal,
                libc::SIGINT | libc::SIGTERM | libc::SIGQUIT | libc::SIGWINCH
            ),
            Defaults::Jvm => signal == libc::SIGQUIT,
        }
    }
}

/// The signals [`hold_ignored`] holds (bit `n - 1` for signal `n`).
static HELD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// [`hold_ignored`] runs once per process.
static HOLD: std::sync::Once = std::sync::Once::new();

/// A held signal's handler: never runs (the signal stays blocked), and does
/// nothing if it does (a thread that started before the hold in a process
/// that is not the CLI).
extern "C" fn held_signal(_: libc::c_int) {}

/// Hold every signal this process inherited ignored (module documentation):
/// block it in this thread, which every thread started later inherits, and
/// give it a handler that does nothing. The CLI calls it first thing, while
/// it is single-threaded; any other process holds them at its first start
/// (crate::spawn_lock), the threads started before then still unblocked
/// (the handler does nothing, `SA_RESTART`). SIGPIPE stays std's (ignored
/// by its runtime, reset by its starts).
pub fn hold_ignored() {
    HOLD.call_once(|| {
        let wanted = (1..=LAST_SIGNAL)
            .filter(|&signal| !matches!(signal, libc::SIGKILL | libc::SIGSTOP | libc::SIGPIPE))
            .filter(|&signal| ignored_now(signal))
            .fold(0u64, |mask, signal| mask | 1 << (signal - 1));
        if wanted == 0 {
            return;
        }
        let mut held = 0u64;
        // SAFETY: sigset_t values are initialized by sigemptyset before
        // use; pthread_sigmask changes only this thread's mask; each
        // sigaction installs a fully initialized action (zeroed, a full
        // mask, SA_RESTART, a handler that does nothing) for a signal that
        // was ignored. Blocked first: no delivery between the two steps.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for signal in signals_of(wanted) {
                libc::sigaddset(&mut set, signal);
            }
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            for signal in signals_of(wanted) {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = held_signal as *const () as libc::sighandler_t;
                action.sa_flags = libc::SA_RESTART;
                libc::sigfillset(&mut action.sa_mask);
                if libc::sigaction(signal, &action, std::ptr::null_mut()) == 0 {
                    held |= 1 << (signal - 1);
                }
            }
            let refused = wanted & !held;
            if refused != 0 {
                // Still ignored: unblocked again, reset in children by hooks.
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                for signal in signals_of(refused) {
                    libc::sigaddset(&mut set, signal);
                }
                libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
            }
        }
        HELD.store(held, std::sync::atomic::Ordering::SeqCst);
    });
}

/// The signals of a bit mask (bit `n - 1` for signal `n`).
fn signals_of(mask: u64) -> impl Iterator<Item = libc::c_int> {
    (1..=LAST_SIGNAL).filter(move |&signal| mask >> (signal - 1) & 1 == 1)
}

/// The held signals ([`hold_ignored`]).
fn held() -> u64 {
    HELD.load(std::sync::atomic::Ordering::SeqCst)
}

/// Whether `signal` is ignored by this process: held ([`hold_ignored`]) or
/// still SIG_IGN.
#[cfg(all(test, target_os = "linux"))]
fn ignored(signal: libc::c_int) -> bool {
    held() >> (signal - 1) & 1 == 1 || ignored_now(signal)
}

/// Whether `signal`'s disposition is SIG_IGN here (false for a number that
/// sigaction refuses, e.g. glibc's reserved real-time signals 32 and 33).
fn ignored_now(signal: libc::c_int) -> bool {
    // SAFETY: with a null new action, sigaction only writes the current
    // disposition into `current`, a zero-initialized (valid) struct.
    unsafe {
        let mut current: libc::sigaction = std::mem::zeroed();
        libc::sigaction(signal, std::ptr::null(), &mut current) == 0
            && current.sa_sigaction == libc::SIG_IGN
    }
}

/// The signals of `defaults` that are ignored in this process, as a bit mask
/// (bit `n - 1` for signal `n`). SIGKILL and SIGSTOP cannot be ignored;
/// SIGPIPE is std's (ignored by its runtime, reset by its spawn).
fn ignored_mask(defaults: Defaults) -> u64 {
    (1..=LAST_SIGNAL)
        .filter(|&signal| !matches!(signal, libc::SIGKILL | libc::SIGSTOP | libc::SIGPIPE))
        .filter(|&signal| defaults.covers(signal) && ignored_now(signal))
        .fold(0, |mask, signal| mask | 1 << (signal - 1))
}

/// The signals `defaults` gives back their default action, as a bit mask.
fn covered_mask(defaults: Defaults) -> u64 {
    (1..=LAST_SIGNAL)
        .filter(|&signal| defaults.covers(signal))
        .fold(0, |mask, signal| mask | 1 << (signal - 1))
}

/// Makes `command`'s process start with the signals of `defaults` that are
/// ignored here at their default action, and the other held ones ignored
/// (module documentation). Call it on the configured command right before
/// it is started or exec'd (crate::spawn_lock::spawn_with does).
pub fn reset_in_child(command: &mut Command, defaults: Defaults) {
    hold_ignored();
    child_dispositions(
        command,
        ignored_mask(defaults),
        held() & !covered_mask(defaults),
    );
}

/// Makes `command`'s process start with the dispositions this process
/// inherited: its held signals ([`hold_ignored`]) ignored again. Every
/// start without [`Defaults`] (crate::spawn_lock::spawn).
pub fn inherit_in_child(command: &mut Command) {
    hold_ignored();
    child_dispositions(command, 0, held());
}

/// The hooks [`child_dispositions`] installed (tests).
#[cfg(test)]
static HOOKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A `pre_exec` hook setting the signals of `reset` to their default action
/// and those of `ignore` to SIG_IGN, when there are any (a hook forces
/// std's fork path: none for the usual start).
fn child_dispositions(command: &mut Command, reset: u64, ignore: u64) {
    if reset == 0 && ignore == 0 {
        return;
    }
    #[cfg(test)]
    HOOKS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let hook = move || {
        for signal in 1..=LAST_SIGNAL {
            let disposition = if reset >> (signal - 1) & 1 == 1 {
                libc::SIG_DFL
            } else if ignore >> (signal - 1) & 1 == 1 {
                libc::SIG_IGN
            } else {
                continue;
            };
            // SAFETY: runs in the child between fork and exec (or right
            // before this process's own exec): only stack values, no
            // allocation or lock; sigemptyset and sigaction are
            // async-signal-safe (signal-safety(7)). `action` is fully
            // initialized (zeroed, then an empty mask and the disposition).
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = disposition;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
        Ok(())
    };
    // SAFETY: the hook is async-signal-safe (above) and touches no state
    // shared with the parent; the masks are moved in by value.
    unsafe {
        command.pre_exec(hook);
    }
}

/// Makes `command` start as the leader of a new session (and so of a new
/// process group): setsid(2) in the child, as Bazel's process-wrapper does
/// before it execs the spawn (process-wrapper-legacy.cc:68-71). The spawn
/// then shares neither the process group nor the session — nor a
/// controlling terminal — with bazel-rs and the shell that started it.
/// Call it instead of `process_group(0)`: setsid fails for a process that
/// already leads its group.
pub fn new_session_in_child(command: &mut Command) {
    // SAFETY: runs in the child between fork and exec: setsid(2) is
    // async-signal-safe (signal-safety(7)) and the hook touches no state
    // shared with the parent; errno is read right after the call.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Bazel's exit code of an interrupted command (ExitCode.INTERRUPTED).
pub const INTERRUPTED_EXIT_CODE: i32 = 8;

extern "C" fn exit_interrupted(_: libc::c_int) {
    // SAFETY: _exit(2) is async-signal-safe; nothing is flushed or dropped,
    // exactly as when the signal's default action ended the process.
    unsafe { libc::_exit(INTERRUPTED_EXIT_CODE) }
}

/// SIGINT (Ctrl-C) ends this process at once with exit code 8, Bazel's
/// INTERRUPTED (the client's Ctrl-C cancels the command, which then exits
/// 8), instead of by the signal. Only when SIGINT is at its default action:
/// an inherited ignored SIGINT (a background job) stays ignored. A handled
/// signal is back at its default action in every process this one starts
/// (exec(2)), and the spawns' supervisors see this process end as before.
/// The progress display's handler (crate::progress::terminal::signals)
/// erases its area, then exits the same way.
pub fn exit_on_interrupt() {
    // SAFETY: a zeroed sigaction is valid; the handler only calls _exit.
    unsafe {
        let mut current: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(libc::SIGINT, std::ptr::null(), &mut current) != 0
            || current.sa_sigaction != libc::SIG_DFL
        {
            return;
        }
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = exit_interrupted as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
    }
}

// Linux only: the tests read signal masks from /proc.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// A `/proc/self/status` mask line's value (`SigIgn:`, `SigBlk:`).
    fn status_mask(text: &str, field: &str) -> u64 {
        let line = text
            .lines()
            .find(|line| line.starts_with(field))
            .unwrap_or_else(|| panic!("no {field} in {text}"));
        u64::from_str_radix(line.split_whitespace().last().unwrap(), 16).unwrap()
    }

    /// The ignored signals among [`PROBED`] and the blocked mask of a `sh`
    /// started through crate::spawn_lock (with `defaults`, or inheriting).
    fn child_status(defaults: Option<Defaults>) -> (Vec<libc::c_int>, u64) {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "grep -E '^Sig(Ign|Blk)' /proc/self/status"]);
        let output = match defaults {
            Some(defaults) => crate::spawn_lock::output_with(&mut command, defaults),
            None => crate::spawn_lock::output(&mut command),
        }
        .unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        let ignored = status_mask(&text, "SigIgn:");
        let ignored = PROBED
            .into_iter()
            .filter(|&signal| ignored >> (signal - 1) & 1 == 1)
            .collect();
        (ignored, status_mask(&text, "SigBlk:"))
    }

    fn child_ignores(defaults: Option<Defaults>) -> Vec<libc::c_int> {
        let (ignored, blocked) = child_status(defaults);
        assert_eq!(blocked, 0, "a started process blocks nothing");
        ignored
    }

    const PROBED: [libc::c_int; 5] = [
        libc::SIGHUP,
        libc::SIGINT,
        libc::SIGQUIT,
        libc::SIGTERM,
        libc::SIGUSR1,
    ];

    /// Hooks installed by [`child_dispositions`] in this process so far.
    fn hooks() -> u64 {
        HOOKS.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[test]
    fn children_get_the_defaults_genuine_bazel_gives_them() {
        // Run in a helper process: dispositions are process-wide, and other
        // tests in this binary start processes concurrently.
        if std::env::var_os("BAZEL_RS_SIGNALS_HELPER").is_none() {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "signals::tests::children_get_the_defaults_genuine_bazel_gives_them",
                    "--nocapture",
                ])
                .env("BAZEL_RS_SIGNALS_HELPER", "1");
            let output = crate::spawn_lock::output(&mut command).unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "{stdout}{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        for signal in PROBED {
            // SAFETY: SIG_IGN with an empty mask; this helper process runs
            // only this test.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = libc::SIG_IGN;
                libc::sigemptyset(&mut action.sa_mask);
                assert_eq!(libc::sigaction(signal, &action, std::ptr::null_mut()), 0);
            }
        }
        let [hup, int, quit, term, usr1] = PROBED;
        let bit = |signal: libc::c_int| 1u64 << (signal - 1);
        let probed = PROBED
            .into_iter()
            .fold(0, |mask, signal| mask | bit(signal));
        // Before the hold: plain SIG_IGN.
        assert_eq!(held(), 0);
        assert_eq!(ignored_mask(Defaults::Jvm), bit(quit));
        assert_eq!(
            ignored_mask(Defaults::Client),
            bit(int) | bit(quit) | bit(term)
        );
        // The first start holds them; every start gets genuine's dispositions.
        assert_eq!(child_ignores(None), PROBED);
        assert_eq!(held() & probed, probed);
        let before = hooks();
        assert_eq!(
            child_ignores(Some(Defaults::ProcessWrapper)),
            Vec::<libc::c_int>::new()
        );
        assert_eq!(hooks(), before, "the defaults need no hook (posix_spawn)");
        assert_eq!(child_ignores(Some(Defaults::Client)), [hup, usr1]);
        assert_eq!(child_ignores(Some(Defaults::Jvm)), [hup, int, term, usr1]);
        assert_eq!(child_ignores(None), PROBED);
        // This process still ignores them: held (blocked here), no longer
        // SIG_IGN.
        assert!(PROBED.into_iter().all(ignored));
        assert_eq!(ignored_mask(Defaults::Jvm), 0);
        let status = std::fs::read_to_string("/proc/thread-self/status").unwrap();
        assert_eq!(status_mask(&status, "SigBlk:") & probed, probed);
        assert_eq!(status_mask(&status, "SigIgn:") & probed, 0);
        // A thread started now inherits the block.
        let status =
            std::thread::spawn(|| std::fs::read_to_string("/proc/thread-self/status").unwrap())
                .join()
                .unwrap();
        assert_eq!(status_mask(&status, "SigBlk:") & probed, probed);
        // A held signal sent to this process does nothing.
        // SAFETY: kill(2) of this process with a signal it holds.
        unsafe {
            libc::kill(libc::getpid(), libc::SIGTERM);
        }
        assert_eq!(child_ignores(None), PROBED);
    }
}
