//! Bazel's UnlimitResources (crate::resource_limits; blaze.cc:1650,
//! blaze_util_posix.cc:855-900): started with a low soft RLIMIT_NOFILE (an
//! interactive shell's 1024 or less), bazel-rs raises the soft limits of
//! RLIMIT_NOFILE and RLIMIT_NPROC to the hard limits at startup, and the
//! actions it spawns inherit the raised limits.
#![allow(clippy::disallowed_methods)] // test harness: starts processes directly (clippy.toml)

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
include!("support/explicit_modules.rs");

struct Workspace(tempfile::TempDir);

impl Workspace {
    fn new(files: &[(&str, &str)]) -> Self {
        let ws = Self(
            tempfile::Builder::new()
                .prefix("resource-limits-")
                .tempdir_in(crate::test_temp::root())
                .unwrap(),
        );
        ws.file("MODULE.bazel", "module(name='resource_limits_fixture')\n");
        for (name, text) in files {
            ws.file(name, text);
        }
        ws
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn file(&self, name: &str, text: &str) {
        let path = self.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// Runs bazel-rs under `ulimit_args` (bash's `ulimit` builtin, applied
    /// to the shell that then execs bazel-rs).
    fn run_limited(&self, ulimit_args: &str, args: &[&str]) -> Output {
        let script = format!("ulimit {ulimit_args} && exec \"$0\" \"$@\"");
        let output = Command::new(TIMEOUT)
            .args(["--kill-after=5s", "600s", "bash", "-c", &script])
            .arg(env!("CARGO_BIN_EXE_bazel-rs"))
            .args(["--ignore_all_rc_files", "--cache-dir=private-cache"])
            .args(args)
            .current_dir(self.path())
            .explicit_modules()
            .output()
            .unwrap();
        assert_ne!(output.status.code(), Some(124), "CLI timed out: {args:?}");
        output
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The hard limits of this test process (bazel-rs inherits them).
fn hard_limits() -> (String, String) {
    let output = Command::new("bash")
        .args(["-c", "ulimit -Hn; ulimit -Hu"])
        .output()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    let mut lines = text.lines().map(str::to_owned);
    (lines.next().unwrap(), lines.next().unwrap())
}

/// Soft RLIMIT_NOFILE 256 and a soft RLIMIT_NPROC below the hard one at
/// startup: the action sees soft == hard for both.
#[test]
fn actions_inherit_soft_limits_raised_to_hard() {
    let (hard_nofile, hard_nproc) = hard_limits();
    let ws = Workspace::new(&[(
        "pkg/BUILD.bazel",
        "genrule(name = 'limits', outs = ['limits.txt'], \
         cmd = '(ulimit -Sn; ulimit -Hn; ulimit -Su; ulimit -Hu) > $@')\n",
    )]);
    // A soft RLIMIT_NPROC just below the hard one (it counts every thread
    // of the user, so a small value would starve the run itself).
    let nproc = match hard_nproc.parse::<u64>() {
        Ok(hard) if hard > 1 => format!(" && ulimit -Su {}", hard - 1),
        Ok(_) => String::new(),
        Err(_) => " && ulimit -Su 4000000".to_owned(),
    };
    let output = ws.run_limited(&format!("-Sn 256{nproc}"), &["build", "//pkg:limits"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let bin = ws.run_limited("-Sn 256", &["info", "bazel-bin"]);
    assert!(bin.status.success(), "{}", stderr(&bin));
    let bin = String::from_utf8(bin.stdout).unwrap();
    let text = fs::read_to_string(Path::new(bin.trim()).join("pkg/limits.txt")).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines,
        [
            hard_nofile.as_str(),
            hard_nofile.as_str(),
            hard_nproc.as_str(),
            hard_nproc.as_str()
        ],
        "action limits (soft/hard nofile, soft/hard nproc)"
    );
}

/// A hard RLIMIT_NOFILE of 256 (nothing to raise) with far more parallel
/// actions than it affords at the old cost of about six descriptors each:
/// the build succeeds (crate::fd_budget: the spawn holds the logs' only
/// descriptors, executing actions are capped to what the limit affords, and
/// a descriptor exhaustion is retried) and no action fails with EMFILE.
#[test]
fn many_parallel_actions_fit_a_hard_limit_of_256() {
    let ws = Workspace::new(&[(
        "pkg/BUILD.bazel",
        "[genrule(name = 'g%d' % i, outs = ['g%d.txt' % i], \
         cmd = 'sleep 0.3; echo %d > $@' % i) for i in range(160)]\n\
         filegroup(name = 'all', srcs = [':g%d' % i for i in range(160)])\n",
    )]);
    let output = ws.run_limited("-n 256", &["build", "//pkg:all", "--jobs=120"]);
    let err = stderr(&output);
    assert!(output.status.success(), "{err}");
    assert!(!err.contains("Too many open files"), "{err}");
    assert!(err.contains("160 processwrapper-sandbox"), "{err}");
}
