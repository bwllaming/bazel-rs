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
//! The reset runs in the child between fork and exec (a `pre_exec` hook that
//! only calls sigaction, which is async-signal-safe), never in this process:
//! bazel-rs itself keeps ignoring what it was told to ignore (nohup, `&`), and
//! the progress display's handlers (crate::progress) stay untouched. The hook
//! is added only when one of the signals is actually ignored here, so the
//! usual start keeps std's posix_spawn fast path (a hook forces fork).
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

/// Whether `signal` is ignored in this process (false for a number that
/// sigaction refuses, e.g. glibc's reserved real-time signals 32 and 33).
fn ignored(signal: libc::c_int) -> bool {
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
        .filter(|&signal| defaults.covers(signal) && ignored(signal))
        .fold(0, |mask, signal| mask | 1 << (signal - 1))
}

/// Makes `command`'s process start with the signals of `defaults` that are
/// ignored here at their default action (module documentation). Call it on
/// the configured command right before it is started or exec'd.
pub fn reset_in_child(command: &mut Command, defaults: Defaults) {
    let mask = ignored_mask(defaults);
    if mask == 0 {
        return;
    }
    let reset = move || {
        for signal in 1..=LAST_SIGNAL {
            if mask >> (signal - 1) & 1 == 0 {
                continue;
            }
            // SAFETY: runs in the child between fork and exec (or right
            // before this process's own exec): only stack values, no
            // allocation or lock; sigemptyset and sigaction are
            // async-signal-safe (signal-safety(7)). `action` is fully
            // initialized (zeroed, then an empty mask and SIG_DFL).
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = libc::SIG_DFL;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
        Ok(())
    };
    // SAFETY: the hook is async-signal-safe (above) and touches no state
    // shared with the parent; `mask` is moved in by value.
    unsafe {
        command.pre_exec(reset);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The ignored signals (SigIgn of /proc/self/status) of a `sh` started
    /// through crate::spawn_lock with `defaults`, among [`PROBED`], which
    /// this process ignores.
    fn child_ignores(defaults: Option<Defaults>) -> Vec<libc::c_int> {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "grep SigIgn /proc/self/status"]);
        if let Some(defaults) = defaults {
            reset_in_child(&mut command, defaults);
        }
        let output = crate::spawn_lock::output(&mut command).unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        let mask = u64::from_str_radix(text.split_whitespace().last().unwrap(), 16).unwrap();
        PROBED
            .into_iter()
            .filter(|&signal| mask >> (signal - 1) & 1 == 1)
            .collect()
    }

    const PROBED: [libc::c_int; 5] = [
        libc::SIGHUP,
        libc::SIGINT,
        libc::SIGQUIT,
        libc::SIGTERM,
        libc::SIGUSR1,
    ];

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
        assert_eq!(child_ignores(None), PROBED);
        assert_eq!(
            child_ignores(Some(Defaults::ProcessWrapper)),
            Vec::<libc::c_int>::new()
        );
        assert_eq!(child_ignores(Some(Defaults::Client)), [hup, usr1]);
        assert_eq!(child_ignores(Some(Defaults::Jvm)), [hup, int, term, usr1]);
        // This process still ignores them.
        assert!(PROBED.into_iter().all(ignored));
        let bit = |signal: libc::c_int| 1u64 << (signal - 1);
        assert_eq!(ignored_mask(Defaults::Jvm), bit(quit));
        assert_eq!(
            ignored_mask(Defaults::Client),
            bit(int) | bit(quit) | bit(term)
        );
    }
}
